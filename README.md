# mdev
mdev daemon workalike, written in pure rust

[mdev](https://git.busybox.net/busybox/plain/docs/mdev.txt) and [mdevd](https://skarnet.org/software/mdevd/) are uevent managers, they listen for kernel object userspace [events](https://www.kernel.org/doc/html/latest/core-api/kobject.html#uevents) and react according to a set of rules that are coded in their `mdev.conf` format.

## Structure and components
Any uevent manager daemon has 4 primary components:
- [x] A netlink listener, in our case [netlink-sys](https://crates.io/crates/netlink-sys) provides us already some level of abstraction
- [x] Something that [parses](https://github.com/rust-italia/kobject-uevent) the messages encoded in the netlink packets
- [x] A set of rules on how to react to the events, we use the same [mdev.conf format](https://github.com/rust-italia/mdev-parser)
- [x] The actual code that reacts to the events according to the rules
  - [x] It matches events with rules
  - [] It executes the actual action and make sure to log the results of it

```
┌─────────┐                 ┌───────────┐
│ Linux   │ Uevent Packets  │ Netlink   │    UEvents
│  Kernel ├────────────────►│  Listener ├─────────────────┐
│         │                 │           │                 ▼
└─────────┘                 └───────────┘            ┌──────────┐
                                                     │ UEvent   │
                                                     │  Daemon  │
┌───────────┐               ┌───────────┐            └───────┬──┘
│ mdev.conf │ mdev lines    │ Rules     │                 ▲  │
│           ├──────────────►│  Parser   ├─────────────────┘  │
│           │               │           │   Rules            │
└───────────┘               └───────────┘                    ▼
                                                    ┌────────────┐
                                                    │ Actions    │
                                                    └────────────┘
```


