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
  that implements `KVM_PRE_FAULT_MEMORY` for them on arm64, which Linux v7.1
  does not.

## Guest requirements

- A guest kernel that supports restricted DMA pools and shares them with the
  host. For Linux, this means `CONFIG_DMA_RESTRICTED_POOL=y` and
  `CONFIG_ARM_PKVM_GUEST=y`.
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
     --cmdline "console=ttyAMA0 root=/dev/vda1 rw" \
     --cpus boot=4 \
     --memory size=1G \
     --disk path=ubuntu.img,image_type=raw
```

## Notes

- Cloud Hypervisor describes a 64 MiB restricted DMA pool in the guest's
  device tree and makes every PCI host bridge refer to it, so that the guest
  bounces device DMA through memory it shares with the host.
- `VIRTIO_F_ACCESS_PLATFORM` is forced on for virtio devices, because a Linux
  guest only routes virtio DMA through the pool when it is negotiated.
- All guest RAM is pre-faulted before the guest starts. Boot time therefore
  grows with the amount of guest RAM, and all of it stays pinned for the life
  of the VM. The pinned memory is charged to the `RLIMIT_MEMLOCK` of the VMM
  unless it holds `CAP_IPC_LOCK` in the initial user namespace.
- No explicit early console (`earlycon=pl011,mmio,...`) is added to the guest
  kernel command line, because accessing the UART that early stalls a
  protected guest. The regular console on `ttyAMA0` works.
- A protected guest is described by a device tree only and receives no PCI
  hotplug notifications. It has to rescan its PCI bus to find an added
  device, and a removed device is ejected as a surprise removal, so the guest
  must release it first. See [hotplug.md](hotplug.md#pci-device-hot-plug).

## Limitations

Cloud Hypervisor does not support the following features with pKVM:

- Memory & CPU hotplug
- Huge Pages (HugeTLB)
- Memory ballooning
- Firmware (UEFI) boot, which has not been validated with pKVM yet
- Guest debugging (gdb)
- Pause, snapshot, restore and live migration
- Hypervisor backends other than KVM
