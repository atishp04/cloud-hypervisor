# Arm pKVM Protected VMs

Protected KVM (pKVM) is a mode of KVM on Arm64 in which a small hypervisor
running at EL2 isolates protected virtual machines (pVMs) from the host. Once
a protected VM is running, neither the host kernel nor the VMM can access its
memory or its vCPU state; the guest shares with the host only the memory it
uses to communicate with the VMM, such as its DMA buffers. Here are some
useful links:

- [pKVM documentation](https://docs.kernel.org/virt/kvm/arm/pkvm.html):
  the Linux kernel's overview of pKVM and of protected VMs.

## Host requirements

- An Arm64 host whose kernel is booted with `kvm-arm.mode=protected`.
- A host kernel that can create protected VMs, through the
  `KVM_VM_TYPE_ARM_PROTECTED` VM type that Linux provides since v7.1, and
  that supports protected guest memory faults. With
  `pkvm_prefault=on`, it must also implement `KVM_PRE_FAULT_MEMORY` for protected
  VMs on arm64, which Linux v7.1 does not.
- With THP-backed guest RAM, the host must support protected share/unshare
  page-table refills when splitting block mappings.

## Guest requirements

- A guest kernel with `CONFIG_ARM_PKVM_GUEST=y` and
  [pKVM confidential-computing support](https://github.com/torvalds/linux/commit/e62decaf98e7c1385c4a22c61ba8bf94d24713a5)
  to share the default SWIOTLB pool with the host.
- Virtio drivers that support `VIRTIO_F_ACCESS_PLATFORM`, as Linux's do.

## Building

pKVM support comes with the KVM backend and needs no extra feature, so the
default build includes it:

```bash
cargo build --release
```

## Running a protected VM

Boot the guest kernel directly and enable pKVM with the `--platform` option:

```bash
./cloud-hypervisor \
     --platform pkvm=on \
     --kernel ./Image \
     --cmdline "console=ttyAMA0 root=/dev/vda1 rw acpi=off swiotlb=32768,8" \
     --cpus boot=4 \
     --memory size=8G,shared=on \
     --disk path=ubuntu.img,image_type=raw
```

## Notes

- The guest uses its default SWIOTLB pool for shared DMA buffers. The guest
  argument `swiotlb=32768,8` selects 64 MiB and eight allocation areas; it
  does not configure the host's pool. Smaller guests can require an explicit
  larger pool size to avoid arm64 reducing its allocation.
- `VIRTIO_F_ACCESS_PLATFORM` is forced on for virtio devices, because a Linux
  guest only routes virtio DMA through the pool when it is negotiated.
- By default, RAM is pinned and mapped on first guest access. Use
  `--platform pkvm=on,pkvm_prefault=on` to pre-fault boot RAM before the
  guest starts. Both settings finalize all vCPUs before guest execution.
  Pre-faulting makes startup time grow with guest RAM and reports pinning errors
  before the guest runs; disabling it defers those costs and errors to first
  access. Pinned memory is charged to the VMM's `RLIMIT_MEMLOCK` unless it holds
  `CAP_IPC_LOCK` in the initial user namespace.
- `pkvm_prefault` is independent of `--memory prefault=on|off`, which controls
  host memory allocation. Leave the latter off to allocate host RAM on demand.
- No explicit early console (`earlycon=pl011,mmio,...`) is added to the guest
  kernel command line, because accessing the UART that early stalls a
  protected guest. The regular console on `ttyAMA0` works.
- With `acpi=off`, the guest uses a device tree and receives no PCI hotplug
  notifications. It has to rescan its PCI bus to find an added
  device, and a removed device is ejected as a surprise removal, so the guest
  must release it first. See [hotplug.md](hotplug.md#pci-device-hot-plug).

## Memory ballooning

Ballooning requires host support for `MEM_RELINQUISH` and a guest kernel with
the pKVM virtio-balloon hooks and `CONFIG_VIRTIO_BALLOON`. The guest relinquishes
each page before the balloon device discards its backing. The host removes
the page's pin, and a later guest access donates private backing again.

With THP, the host must exclude relinquished pages from speculative block
donation. CH retries discard over a complete huge page once every constituent
page has been inflated; earlier subpage holes can retain a host folio while
its neighboring pages are pinned. A partially ballooned huge page can still
retain backing until its remaining pages are released.

Add `--balloon size=0,free_page_reporting=off` at boot. For example, inflate
the balloon to 2 GiB, then deflate it completely:

```bash
ch-remote --api-socket /tmp/ch.sock resize --balloon 2G
ch-remote --api-socket /tmp/ch.sock resize --balloon 0
```

Use `--balloon size=0,free_page_reporting=on` to return unused guest pages
without changing the balloon target. The guest needs `CONFIG_PAGE_REPORTING`
and the Android virtio-balloon changes that relinquish reported pages and
skip DMA mapping for reporting buffers. The virtqueue itself must still use
`VIRTIO_F_ACCESS_PLATFORM` to remain shared. These changes are included in the
`pkvm-grace-latest` Android balloon port; an unmodified upstream guest is not
sufficient.

Reporting is supported without a virtual IOMMU (`iommu=off`, the default).
Reporting descriptors contain guest physical addresses, while CH's vIOMMU
path expects translated device addresses. CH rejects that combination.
Verify that the guest negotiated reporting: Linux can disable it when page
poisoning or `init_on_free` requires the unsupported PAGE_POISON feature.

After reporting completes, the guest can reuse those pages immediately.
Their next access obtains private backing again. Reporting can reduce idle
backing, but acknowledgement alone does not measure physical reclaim;
partially pinned host huge pages can retain backing. Keep reporting enabled
or disabled consistently when comparing balloon timings.

Ordinary inflate/deflate, statistics and `deflate_on_oom` remain available.

## Virtio-mem

Use a guest kernel with `CONFIG_VIRTIO_MEM` and the pKVM memory-relinquish
hook. The host needs the same live `MEM_RELINQUISH` support as ballooning.
The memory window is registered with KVM before guest execution; resize
does not create or remove memory slots.

For 2 GiB boot RAM and a 2 GiB hotplug window, add:

```bash
--memory size=2G,shared=on,thp=on,hotplug_method=virtio-mem,hotplug_size=2G
```

Add `memhp_default_state=online_movable swiotlb=65536,8` to the guest kernel
command line. This selects a 128 MiB default DMA pool with eight areas.
With this kernel and 2 GiB boot RAM, requesting the default 64 MiB size lets
arm64 reduce the pool to 2 MiB, which is too small for protected guest I/O.
Resize specifies the total memory, including boot RAM:

```bash
ch-remote --api-socket /tmp/ch.sock resize --memory 4G
ch-remote --api-socket /tmp/ch.sock resize --memory 2G
```

The guest isolates or offlines pages, relinquishes them to EL2, and then
requests backend discard. Re-plugged memory is donated on first access.
Protected pre-faulting excludes the hotplug window. Control queues negotiate
`VIRTIO_F_ACCESS_PLATFORM` and use shared DMA buffers.

## Limitations

Cloud Hypervisor does not support the following features with pKVM:

- ACPI memory hotplug and CPU hotplug
- Huge Pages (HugeTLB)
- Balloon free-page reporting through a virtual IOMMU
- Firmware (UEFI) boot, which has not been validated with pKVM yet
- Guest debugging (gdb)
- Pause, snapshot, restore and live migration
- Hypervisor backends other than KVM
