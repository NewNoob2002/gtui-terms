# gtui-terms

A terminal UI serial console for Windows and Linux, built for ESP32 work but useful for anything on a UART.

What makes it ESP32-aware:

- Reset / bootloader control through DTR+RTS, with the sequence taken from `esp-idf-monitor` (including the Windows `usbser.sys` workaround where RTS changes are ignored unless DTR is re-written, and the separate USB-JTAG sequence for PID `0x1001`).
- Panic decoding: reads the register dump, filters the code addresses out of the PS/A/SAR/EXCVADDR noise, prints the ELF SHA256, and resolves each address with `addr2line` when you pass `--elf`.
- Core dump capture between the `CORE DUMP START/END` markers: preserves the UART text and decodes the base64 payload into a `.bin` file for `esp-coredump` raw input. Files go beside the log, or into the current directory without `--log`; invalid or incomplete payloads report an error and keep the text capture.
- External command hooks for flashing, `gdb`, or `idf.py coredump`, because those tools want the port to themselves.

Everything else a serial console needs: port picker, 8N1, auto-reconnect, line and raw input, timestamps, raw byte logging, HEX view, scrollback search, timed repeat sends, macros, and a config file.

## Build

```sh
cargo build --release
# -> target/release/gtui-terms
```

Windows: MSVC or the GNU toolchain both work (`rustup override set stable-x86_64-pc-windows-gnu` if you do not have Visual Studio). Linux: `libudev-dev` and `pkg-config` for USB VID/PID on the port picker; musl builds fall back to a sysfs scan.

## Use

```sh
gtui-terms                      # pick a port
gtui-terms COM4 -b 921600       # Windows
gtui-terms /dev/ttyUSB0         # Linux
gtui-terms --demo --panic       # no hardware: simulated boot, progress bar and a panic dump
gtui-terms COM4 --elf build/app.elf --log run.log --timestamps
gtui-terms COM4 --data-bits 7 --parity even --stop-bits 2 --flow hardware
gtui-terms COM4 --no-reset --pipe   # script mode: stdin -> port, port -> stdout
```

Keys: `F1` help, `F2` line/raw toggle, `F5` reset, `F6` bootloader, `F7` search (prefix the query with `re:` for a regex), `F8` hex view, `PageUp/PageDown` scroll, `Ctrl+Home/Ctrl+End` jump to oldest / follow the tail, `Ctrl+N/Ctrl+P` next/previous search hit, `Ctrl+T` timestamps, `Ctrl+L` clear, `Ctrl+Q` quit. In raw mode `Ctrl+]` returns to line mode.

Line commands: `/reset`, `/boot`, `/seq D0 R1 W0.5` (custom DTR/RTS steps, `W` in seconds), `/eol crlf|lf|cr|none`, `/hex 01 0d`, `/filter i,w,e` (send only the listed levels) or `/filter all`, `/repeat <ms> <count|0> <text>`, `/stop`, `/macro <name> <key> <text>`, `/hook <name>`, `/hooks`, `//text` sends a literal leading slash.

The status bar shows the frame format (`921600 baud 8N1`), RX/TX byte totals with a 500 ms RX rate, dropped lines, and the active filter. Command history is kept in `%APPDATA%\gtui-terms\history.txt` (or `$XDG_CONFIG_HOME/gtui-terms/history.txt`). If no `--elf` is given, `build/*.elf` is picked up automatically.

ANSI rendering supports 16/256/RGB foreground and background colours, bold, underline and their resets. Invalid UTF-8 bytes appear as exact `\xNN` escapes; the status bar reports their cumulative count as `invalid UTF-8 N`.

## Config

Looked up as `./.gtui-terms.toml`, then `%APPDATA%\gtui-terms\config.toml` (or `$XDG_CONFIG_HOME`), overridden by `--config`:

```toml
port = "COM4"
baud = 921600
timestamps = true
elf = "build/app.elf"

[[macros]]
name = "version"
key = "alt+v"
text = "at+ver?"

[[hooks]]
name = "flash"
command = "idf.py -p COM4 flash"

[[hooks]]
name = "backtrace"
command = "idf.py -p COM4 coredump"
trigger = "Guru Meditation Error"
```

Create or replace a macro with `/macro version alt+v at+ver?`. It saves to the `--config` file, or `./.gtui-terms.toml`, and the binding works immediately. Keys are `f9`–`f12` or `alt+<char>`; a later definition with the same name overrides an earlier one. Saving preserves other TOML values, including hooks, but normalizes formatting and removes comments. Invalid configurations and failed saves leave the active macro unchanged.

A hook closes the port first, runs the command through `cmd /C` (Windows) or `sh -c`, prints its output into the scrollback, and reconnects. Hooks with a `trigger` regex fire automatically when a received line matches, at most once every five seconds.

## Known limits

No hardware in the development environment, so real-board reset timing, sustained serial throughput and Xtensa/RISC-V symbolization are unverified. ANSI support covers SGR, `ESC[K/D/C/G` and OSC skipping rather than full VT100 emulation. Coredump decoding is byte-verified with synthetic UART captures; real-board dumps have not been checked with `esp-coredump`. No built-in flashing protocol: use hooks.
