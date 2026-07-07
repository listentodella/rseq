# MCU Runtime Patterns

## Contents

- Runtime Contract
- Bytecode VM Shape
- Memory Rules
- Timing Rules
- HAL Boundary
- Firmware Integration

## Runtime Contract

Keep the target runtime small and boring. It should load a validated artifact, maintain bounded state, call board-provided operations, and return deterministic errors.

Recommended runtime API shape:

```rust
pub trait Device {
    type Error;

    fn set_output(&mut self, pin: u8, high: bool) -> Result<(), Self::Error>;
    fn delay_ms(&mut self, ms: u16) -> Result<(), Self::Error>;
    fn read_input(&mut self, pin: u8) -> Result<bool, Self::Error>;
}

pub struct Vm<const STACK: usize> {
    stack: heapless::Vec<i32, STACK>,
    pc: u16,
    fuel: u32,
}

impl<const STACK: usize> Vm<STACK> {
    pub fn step<D: Device>(&mut self, image: &[u8], device: &mut D) -> Result<Step, Error<D::Error>> {
        /* decode one instruction, check bounds, execute */
        todo!()
    }
}
```

Adapt the trait to the real domain. Do not expose broad HAL objects to the VM when a small capability trait is enough.

## Bytecode VM Shape

Use compact fixed-width instructions unless code size strongly favors variable-width encoding. Fixed-width instructions simplify bounds checks and timing analysis.

Common instruction fields:

- `opcode: u8`
- `dst/src/immediate` fields as `u8`, `u16`, or `i16`
- Little-endian encoding documented in one place

Recommended controls:

- Maximum program length.
- Maximum stack/register count.
- Maximum call depth, or no calls.
- Maximum loop iterations or fuel counter.
- No self-modifying code.
- No instruction that can block forever.

For event-driven firmware, make the VM resumable. `step()` should execute one instruction or a bounded chunk and then return `Pending`, `Yield`, `Done`, or `Fault`.

## Memory Rules

Default to `#![no_std]` and avoid `alloc` on the target. Use:

- `heapless` for bounded vectors, strings, queues, and maps.
- `&'static [u8]` for firmware-linked scripts.
- Flash/EEPROM storage with explicit read APIs for updateable scripts.
- Const generics for stack sizes and queue depths.

Reject artifacts that exceed configured limits. Avoid deriving runtime limits from artifact contents without clamping.

## Timing Rules

Document timing assumptions before promising MCU suitability:

- Worst-case cycles per instruction or per state transition.
- Maximum blocking delay allowed by the DSL.
- Whether execution runs in an interrupt, RTIC task, Embassy async task, or main loop.
- Whether DSL actions may touch shared peripherals.

Do not perform long interpreter loops inside interrupt context. In async firmware, model waits as yields or timers, not busy loops.

## HAL Boundary

Make the runtime portable by defining domain traits that firmware implements. Examples:

- GPIO sequencer: `set_output`, `read_input`, `delay_ms`.
- CAN tester: `send_frame`, `recv_match`, `now_ms`.
- Register script: `write_reg`, `read_reg`, `delay_us`.
- Motion/state control: `set_pwm`, `sample_adc`, `fault`.

Return structured errors from both VM and device layers. Preserve enough detail for RTT/defmt logs without requiring formatting in the core runtime.

## Firmware Integration

For existing firmware:

1. Locate the board support crate, HAL, scheduler, and memory map.
2. Decide where the DSL artifact lives: linked bytes, included file, flash partition, EEPROM, or protocol download.
3. Wire `dsl-runtime` into the main loop/task.
4. Add logging behind features: `defmt`, `log`, or no logging.
5. Build for the target and check binary size.
6. Run a hardware smoke test when a probe is available.

For generated Rust instead of bytecode, emit a small function or table module that the firmware imports. Keep generated files deterministic and add them to tests or build output according to the repository's style.
