# macOS AOC input-source write probe (2026-07-25)

## Target

- Mac: Apple Silicon, macOS 26.5, USB-C/DisplayPort Alt Mode
- Display: AOC AG326UZD (`AOC:AG326UZD:XK2R9JA000013`)
- Desktop link: DisplayPort, `ddcutil` bus 4

## Transport result

The fork sends Set VCP `0x60 = 0x10` through `IOAVServiceWriteI2C` twice, with a 10 ms delay before each call. Both calls return `OSStatus 0`. The raw CoreDisplay arguments match m1ddc:

- I²C address: `0x37`
- data address: `0x51`
- bytes: `51 84 03 60 00 10 28`

The leading `0x51` is the DDC/CI data address included in the checksum. `arm.rs` removes it from the payload and passes it separately to CoreDisplay, so the native payload is `84 03 60 00 10 28`. This is the same framing and two-write timing used by m1ddc's SET path.

Initial runs from input `0x0f` did not change input:

```text
before input source: 0x0f
after Mac write input source: 0x0f
after desktop restore input source: 0x0f
```

The desktop can write VCP `0x60` over DisplayPort: `ddcutil setvcp 60 0x0f --bus 4` landed and read back as `0x0f`.

## Discriminator

A harmless brightness write on the same Mac link landed and was restored immediately:

```text
before_brightness=100
after_write_99=99
after_restore_100=100
```

This rules out a generally broken CoreDisplay write path. The input-source result depends on which link is active: the Mac cannot pull the panel away from the desktop while the desktop's DisplayPort link is active. A later run started on `0x10`; writing `0x10` read back `0x10`, and restoring `0x0f` read back `0x0f`.

## Operational consequence

A direct Mac write cannot pull this panel from the desktop input to the Mac input while the desktop link is active. A Mac-initiated claim must use the negotiated path: ask the desktop, which owns the active DisplayPort link, to push VCP `0x60` to the Mac input. Direct write remains a fallback only when readback confirms the requested value; an acknowledged write with mismatched readback is `E_DISPLAY_IO`, not success.
