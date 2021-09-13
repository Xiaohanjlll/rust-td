// Copyright (c) 2021 Intel Corporation
//
// SPDX-License-Identifier: BSD-2-Clause-Patent

// REF: https://github.com/rcore-os/virtio-drivers/blob/master/src/queue.rs

use alloc::collections::btree_map::BTreeMap;
use core::mem::size_of;
use core::slice;
use core::sync::atomic::{fence, Ordering};
use lazy_static::lazy_static;
use spin::Mutex;

use crate::{Result, VirtioError, VirtioTransport};

use bitflags::bitflags;

use volatile::Volatile;

use crate::dma::{virt_to_phys, virtio_dma_alloc, virtio_dma_dealloc, DMA, PAGE_SIZE};

const MAX_QUEUE_SIZE: usize = 32;

/// TD enviroment
/// private memory -> shared memeory
#[derive(Debug, Clone, Copy)]
struct DmaTlbItem {
    pub dma_addr: usize,
    pub phy_addr: usize,
    pub phy_size: usize,
}

impl DmaTlbItem {
    pub fn new(dma_addr: usize, phy_buffer: &[u8]) -> Self {
        DmaTlbItem {
            dma_addr,
            phy_addr: phy_buffer as *const [u8] as *const u8 as usize,
            phy_size: phy_buffer.len(),
        }
    }
}

lazy_static! {
    static ref DMATLB: Mutex<BTreeMap<usize, DmaTlbItem>> = Mutex::new(BTreeMap::new());
}
/// The mechanism for bulk data transport on virtio devices.
///
/// Each device can have zero or more virtqueues.
#[repr(C)]
pub struct VirtQueue {
    /// DMA guard
    dma: DMA,
    /// Descriptor table
    desc: &'static mut [Descriptor],
    /// Available ring
    avail: &'static mut AvailRing,
    /// Used ring
    used: &'static mut UsedRing,

    /// The index of queue
    queue_idx: u32,
    /// The size of queue
    queue_size: u16,
    /// The number of used queues.
    num_used: u16,
    /// The head desc index of the free list.
    free_head: u16,
    avail_idx: u16,
    last_used_idx: u16,
}

impl VirtQueue {
    /// Create a new VirtQueue.
    pub fn new(header: &dyn VirtioTransport, idx: usize, size: u16) -> Result<Self> {
        // TBD: add get_descriptors_address for VirtioTransport
        // if header.queue_used(idx as u32) {
        //     return Err(VirtioError::AlreadyUsed);
        // }
        if !size.is_power_of_two()
            || header.get_queue_max_size() < size
            || size > MAX_QUEUE_SIZE as u16
        {
            return Err(VirtioError::InvalidParam);
        }
        let layout = VirtQueueLayout::new(size);
        // alloc continuous pages
        let dma = DMA::new(layout.size / PAGE_SIZE)?;

        // header.queue_set(idx as u32, size as u32, PAGE_SIZE as u32, dma.pfn());
        header.set_descriptors_address(dma.paddr() as u64);
        header.set_avail_ring(dma.paddr() as u64 + layout.avail_offset as u64);
        header.set_used_ring(dma.paddr() as u64 + layout.used_offset as u64);

        let desc: &'static mut [Descriptor] =
            unsafe { slice::from_raw_parts_mut(dma.vaddr() as *mut Descriptor, size as usize) };
        let avail: &'static mut AvailRing =
            unsafe { &mut *((dma.vaddr() + layout.avail_offset) as *mut AvailRing) };
        let used: &'static mut UsedRing =
            unsafe { &mut *((dma.vaddr() + layout.used_offset) as *mut UsedRing) };

        // link descriptors together
        for i in 0..(size - 1) {
            desc[i as usize].next.write(i + 1);
        }

        Ok(VirtQueue {
            dma,
            desc,
            avail,
            used,
            queue_size: size,
            queue_idx: idx as u32,
            num_used: 0,
            free_head: 0,
            avail_idx: 0,
            last_used_idx: 0,
        })
    }

    /// Add buffers to the virtqueue, return a token.
    ///
    /// Ref: linux virtio_ring.c virtqueue_add
    pub fn add(&mut self, inputs: &[&[u8]], outputs: &[&mut [u8]]) -> Result<u16> {
        if inputs.is_empty() && outputs.is_empty() {
            return Err(VirtioError::InvalidParam);
        }
        if inputs.len() + outputs.len() + self.num_used as usize > self.queue_size as usize {
            return Err(VirtioError::BufferTooSmall);
        }

        // allocate descriptors from free list
        let head = self.free_head;
        let mut last = self.free_head;
        for input in inputs.iter() {
            let input = unsafe {
                let dma_addr = virtio_dma_alloc(64);
                let dma_input = core::slice::from_raw_parts_mut(dma_addr as *mut u8, input.len());
                dma_input.copy_from_slice(input);
                dma_input
            };
            let desc = &mut self.desc[self.free_head as usize];
            desc.set_buf(input);
            desc.flags.write(DescFlags::NEXT);
            last = self.free_head;
            self.free_head = desc.next.read();
        }

        for output in outputs.iter() {
            let output = unsafe {
                let dma_addr = virtio_dma_alloc(64);
                if dma_addr == 0 {
                    return Err(VirtioError::DmaError);
                }
                let dma_output =
                    core::slice::from_raw_parts_mut(dma_addr as *mut u8, 64 * PAGE_SIZE);

                let dma_tlb = DmaTlbItem::new(dma_addr, output);
                if DMATLB.lock().contains_key(&dma_addr) {
                    return Err(VirtioError::DmaError);
                }
                DMATLB.lock().insert(dma_addr, dma_tlb);
                dma_output
            };
            let desc = &mut self.desc[self.free_head as usize];
            desc.set_buf(output);
            desc.flags.write(DescFlags::NEXT | DescFlags::WRITE);
            last = self.free_head;
            self.free_head = desc.next.read();
        }
        // set last_elem.next = NULL
        {
            let desc = &mut self.desc[last as usize];
            let mut flags = desc.flags.read();
            flags.remove(DescFlags::NEXT);
            desc.flags.write(flags);
        }
        self.num_used += (inputs.len() + outputs.len()) as u16;

        let avail_slot = self.avail_idx & (self.queue_size - 1);
        self.avail.ring[avail_slot as usize].write(head);

        // write barrier
        fence(Ordering::SeqCst);

        // increase head of avail ring
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.avail.idx.write(self.avail_idx);
        Ok(head)
    }

    /// Whether there is a used element that can pop.
    pub fn can_pop(&self) -> bool {
        self.last_used_idx != self.used.idx.read()
    }

    /// The number of free descriptors.
    pub fn available_desc(&self) -> usize {
        (self.queue_size - self.num_used) as usize
    }

    /// Recycle descriptors in the list specified by head.
    ///
    /// This will push all linked descriptors at the front of the free list.
    fn recycle_descriptors(&mut self, mut head: u16) {
        let origin_free_head = self.free_head;
        self.free_head = head;
        loop {
            let desc = &mut self.desc[head as usize];

            // Copy result from shared memory to private memory.
            let addr = desc.addr.read();
            unsafe {
                // Only cache DMA for rx
                if let Some(dma_tlb) = DMATLB.lock().remove(&(addr as usize)) {
                    let dst = core::slice::from_raw_parts_mut(
                        dma_tlb.phy_addr as *mut u8,
                        dma_tlb.phy_size,
                    );
                    let src = core::slice::from_raw_parts(
                        dma_tlb.dma_addr as *const u8,
                        dma_tlb.phy_size,
                    );
                    dst.copy_from_slice(src);
                }
                // Free all DMA
                virtio_dma_dealloc(addr as usize, 64);
            }

            let flags = desc.flags.read();
            self.num_used -= 1;
            if flags.contains(DescFlags::NEXT) {
                head = desc.next.read();
            } else {
                desc.next.write(origin_free_head);
                return;
            }
        }
    }

    /// Get a token from device used buffers, return (token, len).
    ///
    /// Ref: linux virtio_ring.c virtqueue_get_buf_ctx
    pub fn pop_used(&mut self) -> Result<(u16, u32)> {
        if !self.can_pop() {
            return Err(VirtioError::NotReady);
        }
        // read barrier
        fence(Ordering::SeqCst);

        let last_used_slot = self.last_used_idx & (self.queue_size - 1);
        let index = self.used.ring[last_used_slot as usize].id.read() as u16;
        let len = self.used.ring[last_used_slot as usize].len.read();

        self.recycle_descriptors(index);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        Ok((index, len))
    }
}

/// The inner layout of a VirtQueue.
struct VirtQueueLayout {
    avail_offset: usize,
    used_offset: usize,
    size: usize,
}

/// Align `size` up to a page.
fn align_up(size: usize) -> usize {
    (size + PAGE_SIZE) & !(PAGE_SIZE - 1)
}

impl VirtQueueLayout {
    fn new(queue_size: u16) -> Self {
        assert!(
            queue_size.is_power_of_two(),
            "queue size should be a power of 2"
        );
        let queue_size = queue_size as usize;
        let desc = size_of::<Descriptor>() * queue_size;
        let avail = size_of::<u16>() * (3 + queue_size);
        let used = size_of::<u16>() * 3 + size_of::<UsedElem>() * queue_size;
        VirtQueueLayout {
            avail_offset: desc,
            used_offset: align_up(desc + avail),
            size: align_up(desc + avail) + align_up(used),
        }
    }
}

#[repr(C, align(16))]
#[derive(Debug)]
struct Descriptor {
    addr: Volatile<u64>,
    len: Volatile<u32>,
    flags: Volatile<DescFlags>,
    next: Volatile<u16>,
}

impl Descriptor {
    fn set_buf(&mut self, buf: &[u8]) {
        self.addr.write(virt_to_phys(buf.as_ptr() as usize) as u64);
        self.len.write(buf.len() as u32);
    }
}

bitflags! {
    /// Descriptor flags
    struct DescFlags: u16 {
        const NEXT = 1;
        const WRITE = 2;
        const INDIRECT = 4;
    }
}

/// The driver uses the available ring to offer buffers to the device:
/// each ring entry refers to the head of a descriptor chain.
/// It is only written by the driver and read by the device.
#[repr(C)]
#[derive(Debug)]
struct AvailRing {
    flags: Volatile<u16>,
    /// A driver MUST NOT decrement the idx.
    idx: Volatile<u16>,
    ring: [Volatile<u16>; MAX_QUEUE_SIZE], // actual size: queue_size
    used_event: Volatile<u16>,             // unused
}

/// The used ring is where the device returns buffers once it is done with them:
/// it is only written to by the device, and read by the driver.
#[repr(C)]
#[derive(Debug)]
struct UsedRing {
    flags: Volatile<u16>,
    idx: Volatile<u16>,
    ring: [UsedElem; MAX_QUEUE_SIZE], // actual size: queue_size
    avail_event: Volatile<u16>,       // unused
}

#[repr(C)]
#[derive(Debug)]
struct UsedElem {
    id: Volatile<u32>,
    len: Volatile<u32>,
}
