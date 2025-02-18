//! Rust Boot Services Allocator
//!
//! Implements a global allocator based on UEFI AllocatePool().
//! Memory is allocated from the EFI_BOOT_SERVICES_DATA pool.
//!
//! ## Examples and Usage
//!
//! ```no_run
//! use r_efi::efi::Status;
//! pub extern "efiapi" fn efi_main(
//!   _image_handle: *const core::ffi::c_void,
//!   system_table: *const r_efi::system::SystemTable,
//! ) -> u64 {
//!   rust_boot_services_allocator_dxe::GLOBAL_ALLOCATOR.init(unsafe { (*system_table).boot_services});
//!
//!   let mut foo = vec!["asdf", "xyzpdq", "abcdefg", "theoden"];
//!   foo.sort();
//!
//!   let bar = Box::new(foo);
//!
//!   Status::SUCCESS.as_usize() as u64
//! }
//! ```
//!
//! ## License
//!
//! Copyright (C) Microsoft Corporation. All rights reserved.
//!
//! SPDX-License-Identifier: BSD-2-Clause-Patent
//!
#![no_std]
#![feature(allocator_api)]

use core::{
  alloc::{GlobalAlloc, Layout},
  ffi::c_void,
};

use r_efi::{
  efi::{BootServices, Status},
  system::BOOT_SERVICES_DATA,
};

/// Static GLOBAL_ALLOCATOR instance that is marked with the `#[global_allocator]` attribute.
///
/// See [`core::alloc::GlobalAlloc`] for more details on rust global allocators. This allocator must be initialized
/// before use, see [`SpinLockedAllocator::init()`].
#[cfg_attr(not(test), global_allocator)]
pub static GLOBAL_ALLOCATOR: SpinLockedAllocator = SpinLockedAllocator::new();

const ALLOC_TRACKER_SIG: u32 = 0x706F6F6C; //arbitrary sig

// Used to track allocations that need larger alignment than the UEFI Pool alignment (8 bytes).
struct AllocationTracker {
  signature: u32,
  orig_ptr: *mut c_void,
}

// Private unlocked allocator implementation. The public locked allocator delegates to this implementation.
struct BootServicesAllocator {
  boot_services: Option<*mut BootServices>,
}

impl BootServicesAllocator {
  // Create a new instance. const fn to allow static initialization.
  const fn new() -> Self {
    BootServicesAllocator { boot_services: None }
  }

  // initialize the allocator by providing a pointer to the global boot services table.
  fn init(&mut self, boot_services: *mut BootServices) {
    self.boot_services = Some(boot_services);
  }

  // implement allocation using EFI boot services AllocatePool() call.
  fn boot_services_alloc(&self, layout: Layout) -> *mut u8 {
    //bail early if not initialized.
    let Some(bs_ptr) = self.boot_services else { return core::ptr::null_mut() };

    let bs = unsafe { bs_ptr.as_mut().expect("Boot Services pointer is null.") };

    match layout.align() {
      0..=8 => {
        //allocate the pointer directly since UEFI pool allocations are 8-byte aligned already.
        let mut ptr: *mut c_void = core::ptr::null_mut();
        match (bs.allocate_pool)(BOOT_SERVICES_DATA, layout.size(), core::ptr::addr_of_mut!(ptr)) {
          Status::SUCCESS => ptr as *mut u8,
          _ => core::ptr::null_mut(),
        }
      }
      _ => {
        //allocate extra space to align the allocation as requested and include a tracking structure to allow
        //recovery of the original pointer for de-allocation. Tracking structure follows the allocation.
        let (expanded_layout, tracking_offset) = match layout.extend(Layout::new::<AllocationTracker>()) {
          Ok(x) => x,
          Err(_) => return core::ptr::null_mut(),
        };
        let expanded_size = expanded_layout.size() + expanded_layout.align();

        let mut orig_ptr: *mut c_void = core::ptr::null_mut();
        let final_ptr = match (bs.allocate_pool)(BOOT_SERVICES_DATA, expanded_size, core::ptr::addr_of_mut!(orig_ptr)) {
          Status::SUCCESS => orig_ptr as *mut u8,
          _ => return core::ptr::null_mut(),
        };

        //align the pointer up to the required alignment.
        let final_ptr = unsafe { final_ptr.add(final_ptr.align_offset(expanded_layout.align())) };

        //get a reference to the allocation tracking structure after the allocation and populate it.
        let tracker = unsafe {
          final_ptr.add(tracking_offset).cast::<AllocationTracker>().as_mut().expect("tracking pointer is invalid")
        };

        tracker.signature = ALLOC_TRACKER_SIG;
        tracker.orig_ptr = orig_ptr;

        final_ptr
      }
    }
  }

  // implement dealloc (free) using EFI boot services FreePool() call.
  fn boot_services_dealloc(&self, ptr: *mut u8, layout: Layout) {
    //bail early if not initialized.
    let Some(bs_ptr) = self.boot_services else { return };

    let bs = unsafe { bs_ptr.as_mut().expect("Boot Services pointer is null.") };

    match layout.align() {
      0..=8 => {
        //pointer was allocated directly, so free it directly.
        let _ = (bs.free_pool)(ptr as *mut c_void);
      }
      _ => {
        //pointer was potentially adjusted for alignment. Recover tracking structure to retrieve the original
        //pointer to free.
        let (_, tracking_offset) = match layout.extend(Layout::new::<AllocationTracker>()) {
          Ok(x) => x,
          Err(_) => return,
        };
        let tracker = unsafe {
          ptr.add(tracking_offset).cast::<AllocationTracker>().as_mut().expect("tracking pointer is invalid")
        };
        debug_assert_eq!(tracker.signature, ALLOC_TRACKER_SIG);
        let _ = (bs.free_pool)(tracker.orig_ptr);
      }
    }
  }
}

/// A spin-locked allocator implementation.
///
/// Provides a locked allocator instance (using [`spin::Mutex`]) that is suitable for use as a
/// [`core::alloc::GlobalAlloc`].
pub struct SpinLockedAllocator {
  inner: spin::Mutex<BootServicesAllocator>,
}

impl SpinLockedAllocator {
  // Create a new instance. const fn to allow static initialization.
  const fn new() -> Self {
    SpinLockedAllocator { inner: spin::Mutex::new(BootServicesAllocator::new()) }
  }

  /// Initialize the allocator.
  ///
  /// This routine initializes the allocator by providing a pointer to the global EFI Boot Services table that will
  /// supply pointers to the AllocatePool() and FreePool() primitives that implement memory allocation.
  pub fn init(&self, boot_services: *mut BootServices) {
    self.inner.lock().init(boot_services);
  }
}

unsafe impl GlobalAlloc for SpinLockedAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    self.inner.lock().boot_services_alloc(layout)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    self.inner.lock().boot_services_dealloc(ptr, layout)
  }
}

unsafe impl Sync for SpinLockedAllocator {}
unsafe impl Send for SpinLockedAllocator {}

#[cfg(test)]
mod tests {
  extern crate std;

  use core::{
    alloc::{GlobalAlloc, Layout},
    ffi::c_void,
    mem::MaybeUninit,
  };
  use std::alloc::System;

  use r_efi::{
    efi::Status,
    system::{BootServices, BOOT_SERVICES_DATA},
  };
  use std::collections::BTreeMap;

  use crate::{AllocationTracker, SpinLockedAllocator, ALLOC_TRACKER_SIG};

  static ALLOCATION_TRACKER: spin::Mutex<BTreeMap<usize, Layout>> = spin::Mutex::new(BTreeMap::new());

  extern "efiapi" fn mock_allocate_pool(
    pool_type: r_efi::system::MemoryType,
    size: usize,
    buffer: *mut *mut c_void,
  ) -> Status {
    assert_eq!(pool_type, BOOT_SERVICES_DATA);

    unsafe {
      let layout = Layout::from_size_align(size, 8).unwrap();
      let ptr = System.alloc(layout) as *mut c_void;
      buffer.write(ptr);
      let existing_key = ALLOCATION_TRACKER.lock().insert(ptr as usize, layout);
      assert!(existing_key.is_none());
    }

    Status::SUCCESS
  }

  extern "efiapi" fn mock_free_pool(buffer: *mut c_void) -> Status {
    let layout = ALLOCATION_TRACKER.lock().remove(&(buffer as usize)).expect("freeing an un-allocated pointer");
    unsafe {
      System.dealloc(buffer as *mut u8, layout);
    }

    Status::SUCCESS
  }

  fn mock_boot_services() -> BootServices {
    let boot_services = MaybeUninit::zeroed();
    let mut boot_services: BootServices = unsafe { boot_services.assume_init() };
    boot_services.allocate_pool = mock_allocate_pool;
    boot_services.free_pool = mock_free_pool;
    boot_services
  }

  #[test]
  fn basic_alloc_and_dealloc() {
    static ALLOCATOR: SpinLockedAllocator = SpinLockedAllocator::new();
    ALLOCATOR.init(&mut mock_boot_services());

    let layout = Layout::from_size_align(0x40, 0x8).unwrap();
    let ptr = unsafe { ALLOCATOR.alloc_zeroed(layout) };
    assert!(!ptr.is_null());
    assert!(ALLOCATION_TRACKER.lock().contains_key(&(ptr as usize)));

    unsafe { ALLOCATOR.dealloc(ptr, layout) };
    assert!(!ALLOCATION_TRACKER.lock().contains_key(&(ptr as usize)));
  }

  #[test]
  fn big_alignment_should_allocate_tracking_structure() {
    static ALLOCATOR: SpinLockedAllocator = SpinLockedAllocator::new();
    ALLOCATOR.init(&mut mock_boot_services());

    let layout = Layout::from_size_align(0x40, 0x1000).unwrap();
    let ptr = unsafe { ALLOCATOR.alloc_zeroed(layout) };
    assert!(!ptr.is_null());
    assert_eq!(ptr.align_offset(0x1000), 0);

    // reconstruct a reference to the tracker structure at the end of the allocation.
    let (_, tracking_offset) = layout.extend(Layout::new::<AllocationTracker>()).unwrap();
    let tracker =
      unsafe { ptr.add(tracking_offset).cast::<AllocationTracker>().as_mut().expect("tracking pointer is invalid") };
    assert_eq!(tracker.signature, ALLOC_TRACKER_SIG);

    let orig_ptr_addr = tracker.orig_ptr as usize;

    assert!(ALLOCATION_TRACKER.lock().contains_key(&(orig_ptr_addr)));

    unsafe { ALLOCATOR.dealloc(ptr, layout) };

    assert!(!ALLOCATION_TRACKER.lock().contains_key(&(orig_ptr_addr)));
  }
}
