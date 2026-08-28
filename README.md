# pipemeeter-core

PipeWire handling for a fixed layout mixer. It creates the virtual devices,
starts a filter chain helper per strip, manages links, reads meters, and
remembers devices that are not currently plugged in.

Nothing here draws anything.

## Building

Needs the `libpipewire-0.3` headers and clang at build time. The `pipewire`
crate must stay at 0.10 or newer, and needs its `v0_3_44` feature: without it,
keys such as `TARGET_OBJECT` are not compiled in, which fails at build time
rather than at run time.

## Two traps worth knowing about

A filter chain drops control writes while it is idle. It accepts the message
and discards it. Levels have to go out after the chain is linked to something
that drives it, never before. Getting this backwards looks exactly like a
control naming problem and can be chased for a long time.

Removing and creating devices in the same frame is a race, and it is lost the
obvious way. Both are requests to the PipeWire thread, not things that have
happened by the time the call returns. Ask for the removal, wait until the
nodes are actually gone, then ask for the creation.

## Devices that are not there

An assignment to an absent device is kept rather than cleared, so unplugging a
headset and plugging it back in restores the routing on its own. The registry
holds the description too, so a caller can still show the device's name.

## License

Public domain. See UNLICENSE.
