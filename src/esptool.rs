//! Built-in ESP flashing via the `espflash` library: no Python, esptool or ESP-IDF needed.
//!
//! Inputs are validated on the UI thread (`build`) so destructive operations can be
//! previewed exactly; `run` then executes on a worker thread with the port released.

use std::fs::File;
use std::path::{Path, PathBuf};

use espflash::connection::{Connection, ResetAfterOperation, ResetBeforeOperation};
use espflash::flasher::{FlashData, FlashSettings, Flasher};
use espflash::image_format::Segment;
use espflash::image_format::idf::IdfBootloaderFormat;
use espflash::target::ProgressCallbacks;

/// 烧录时切换到的默认波特率；CH340/CP210x 在 460800 下普遍稳定。
pub const DEFAULT_FLASH_BAUD: u32 = 460_800;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    ChipId,
    FlashId,
    ReadMac,
    EraseFlash,
    WriteFlash,
    /// ESP-IDF build directory / flasher_args.json, or an application ELF.
    Flash,
}

impl Operation {
    pub fn label(self) -> &'static str {
        match self {
            Self::ChipId => "chip-id",
            Self::FlashId => "flash-id",
            Self::ReadMac => "read-mac",
            Self::EraseFlash => "erase-flash",
            Self::WriteFlash => "write-flash",
            Self::Flash => "flash",
        }
    }

    pub fn destructive(self) -> bool {
        matches!(self, Self::EraseFlash | Self::WriteFlash | Self::Flash)
    }
}

#[derive(Debug)]
enum Job {
    Query,
    Erase,
    Images(Vec<(u32, PathBuf)>),
    Elf {
        elf: PathBuf,
        bootloader: Option<PathBuf>,
        partition_table: Option<PathBuf>,
    },
}

#[derive(Debug)]
pub struct Invocation {
    port: String,
    baud: u32,
    operation: Operation,
    job: Job,
    pub summary: String,
}

pub fn build(
    port: &str,
    baud: u32,
    operation: Operation,
    arguments: &str,
) -> Result<Invocation, String> {
    if port.trim().is_empty() || port.starts_with('-') || port.contains('\0') {
        return Err("select a valid serial port before running ESP tools".into());
    }
    if baud == 0 {
        return Err("flash baud rate must be greater than zero".into());
    }
    if !matches!(operation, Operation::WriteFlash | Operation::Flash)
        && !arguments.trim().is_empty()
    {
        return Err(format!("{} does not accept arguments", operation.label()));
    }
    let job = match operation {
        Operation::ChipId | Operation::FlashId | Operation::ReadMac => Job::Query,
        Operation::EraseFlash => Job::Erase,
        Operation::WriteFlash => {
            let words = split_arguments(arguments)?;
            if words.is_empty() || words.len() % 2 != 0 {
                return Err("write-flash requires one or more <offset> <file> pairs; quote paths containing spaces".into());
            }
            let mut pairs = Vec::with_capacity(words.len() / 2);
            for pair in words.as_chunks::<2>().0 {
                let offset = parse_offset(&pair[0])?;
                if pair[1].is_empty() || pair[1].starts_with('-') || pair[1].contains('\0') {
                    return Err(
                        "each flash offset must be followed by an image path, not an option".into(),
                    );
                }
                pairs.push((offset, PathBuf::from(&pair[1])));
            }
            Job::Images(check_images(pairs)?)
        }
        Operation::Flash => {
            let words = split_arguments(arguments)?;
            let [target] = words.as_slice() else {
                return Err("usage: /esp flash <build dir | flasher_args.json | app.elf>".into());
            };
            plan_project(Path::new(target))?
        }
    };
    let mut summary = format!(
        "{} on {port} at {baud} baud (built-in espflash)",
        operation.label()
    );
    match &job {
        Job::Images(images) => {
            for (offset, path) in images {
                let size = path.metadata().map(|meta| meta.len()).unwrap_or(0);
                summary.push_str(&format!(
                    "\n  0x{offset:06x}  {}  ({size} B)",
                    path.display()
                ));
            }
        }
        Job::Elf {
            elf,
            bootloader,
            partition_table,
        } => {
            summary.push_str(&format!("\n  app ELF      {}", elf.display()));
            summary.push_str(&match bootloader {
                Some(path) => format!("\n  bootloader   {}", path.display()),
                None => "\n  bootloader   espflash default for the detected chip".to_string(),
            });
            summary.push_str(&match partition_table {
                Some(path) => format!("\n  partitions   {}", path.display()),
                None => {
                    "\n  partitions   generated default table (nvs, phy_init, factory)".to_string()
                }
            });
        }
        Job::Erase => summary.push_str("\n  erases the entire Flash chip"),
        Job::Query => {}
    }
    Ok(Invocation {
        port: port.to_string(),
        baud,
        operation,
        job,
        summary,
    })
}

/// 输入是否为单个路径（一键烧录），而非 `<offset> <file>` 成对参数。
pub fn is_single_path(arguments: &str) -> bool {
    split_arguments(arguments).is_ok_and(|words| words.len() == 1)
}

/// 校验镜像存在、非空、可读、不重叠且不越过 32 位地址，返回规范化路径。
fn check_images(pairs: Vec<(u32, PathBuf)>) -> Result<Vec<(u32, PathBuf)>, String> {
    let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(pairs.len());
    let mut images = Vec::with_capacity(pairs.len());
    for (offset, path) in pairs {
        let image = File::open(&path)
            .map_err(|err| format!("cannot read image {}: {err}", path.display()))?;
        let metadata = image
            .metadata()
            .map_err(|err| format!("cannot inspect image {}: {err}", path.display()))?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(format!(
                "image {} must be a nonempty regular file",
                path.display()
            ));
        }
        let end = u64::from(offset)
            .checked_add(metadata.len())
            .filter(|end| *end <= u64::from(u32::MAX) + 1)
            .ok_or_else(|| {
                format!(
                    "image {} exceeds the 32-bit flash address range",
                    path.display()
                )
            })?;
        if ranges
            .iter()
            .any(|&(start, stop)| u64::from(offset) < stop && start < end)
        {
            return Err(format!(
                "image {} overlaps another flash image",
                path.display()
            ));
        }
        ranges.push((u64::from(offset), end));
        let path = path
            .canonicalize()
            .map_err(|err| format!("cannot resolve image {}: {err}", path.display()))?;
        images.push((offset, path));
    }
    Ok(images)
}

/// 解析一键烧录目标：ESP-IDF 构建目录或其 flasher_args.json 按其中的偏移烧录全部镜像；
/// ELF 由 espflash 生成应用镜像，并优先使用同一构建目录下的 bootloader/分区表，
/// 找不到时使用 espflash 内置 bootloader 与默认分区表，用户无需手写分区表。
fn plan_project(target: &Path) -> Result<Job, String> {
    let metadata = target
        .metadata()
        .map_err(|err| format!("cannot inspect {}: {err}", target.display()))?;
    let args_file = if metadata.is_dir() {
        let candidate = target.join("flasher_args.json");
        if candidate.is_file() {
            Some(candidate)
        } else {
            let elf = single_elf(target)?;
            return plan_project(&elf);
        }
    } else if target
        .file_name()
        .is_some_and(|name| name == "flasher_args.json")
    {
        Some(target.to_path_buf())
    } else {
        None
    };
    if let Some(args_file) = args_file {
        let text = std::fs::read_to_string(&args_file)
            .map_err(|err| format!("cannot read {}: {err}", args_file.display()))?;
        let base = args_file.parent().unwrap_or(Path::new("."));
        let pairs = flash_files(&text)
            .map_err(|err| format!("{}: {err}", args_file.display()))?
            .into_iter()
            .map(|(offset, file)| (offset, base.join(file)))
            .collect();
        return Ok(Job::Images(check_images(pairs)?));
    }
    let mut magic = [0u8; 4];
    std::io::Read::read_exact(
        &mut File::open(target)
            .map_err(|err| format!("cannot read {}: {err}", target.display()))?,
        &mut magic,
    )
    .map_err(|err| format!("cannot read {}: {err}", target.display()))?;
    if magic != *b"\x7fELF" {
        return Err(format!(
            "{} is not an ELF; use /esp write-flash <offset> <file> for raw .bin images",
            target.display()
        ));
    }
    let dir = target.parent().unwrap_or(Path::new("."));
    let existing = |path: PathBuf| path.is_file().then_some(path);
    Ok(Job::Elf {
        elf: target
            .canonicalize()
            .map_err(|err| format!("cannot resolve {}: {err}", target.display()))?,
        bootloader: existing(dir.join("bootloader").join("bootloader.bin")),
        partition_table: existing(dir.join("partition_table").join("partition-table.bin")),
    })
}

fn single_elf(dir: &Path) -> Result<PathBuf, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|err| format!("cannot list {}: {err}", dir.display()))?;
    let elves: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "elf"))
        .collect();
    match elves.as_slice() {
        [elf] => Ok(elf.clone()),
        [] => Err(format!(
            "{} has no flasher_args.json or .elf; build the project first",
            dir.display()
        )),
        _ => Err(format!(
            "{} has several .elf files; pass the application ELF explicitly",
            dir.display()
        )),
    }
}

/// 从 ESP-IDF 生成的 flasher_args.json 中取出 `"flash_files": { "0x1000": "path", ... }`。
/// 该文件结构固定且简单，这里只做最小解析，避免为此引入 JSON 依赖。
fn flash_files(text: &str) -> Result<Vec<(u32, String)>, String> {
    let start = text
        .find("\"flash_files\"")
        .ok_or("missing \"flash_files\"")?;
    let rest = &text[start..];
    let open = rest.find('{').ok_or("malformed \"flash_files\"")?;
    let close = rest[open..].find('}').ok_or("malformed \"flash_files\"")? + open;
    let strings = json_strings(&rest[open + 1..close])?;
    if strings.is_empty() || strings.len() % 2 != 0 {
        return Err("\"flash_files\" must map offsets to files".into());
    }
    strings
        .chunks(2)
        .map(|pair| Ok((parse_offset(&pair[0])?, pair[1].clone())))
        .collect()
}

fn json_strings(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '"' {
            continue;
        }
        let mut value = String::new();
        loop {
            match chars.next().ok_or("unterminated string")? {
                '"' => break,
                '\\' => match chars.next().ok_or("unterminated string")? {
                    'n' => value.push('\n'),
                    't' => value.push('\t'),
                    other => value.push(other),
                },
                other => value.push(other),
            }
        }
        out.push(value);
    }
    Ok(out)
}

/// 进度按 10% 步进输出，避免刷屏。
struct Progress<'a, F: Fn(String)> {
    on_output: &'a F,
    /// 按写入顺序的段地址：espflash 跳过未变化的段时不调用 `init`，只能由此得知地址。
    segments: Vec<u32>,
    index: usize,
    addr: u32,
    total: usize,
    last_step: usize,
}

impl<F: Fn(String)> ProgressCallbacks for Progress<'_, F> {
    fn init(&mut self, addr: u32, total: usize) {
        self.addr = addr;
        self.total = total.max(1);
        self.last_step = 0;
        (self.on_output)(format!("0x{addr:06x}: writing {total} block(s)…"));
    }

    fn update(&mut self, current: usize) {
        let step = current * 10 / self.total;
        if step > self.last_step {
            self.last_step = step;
            (self.on_output)(format!("0x{:06x}: {}%", self.addr, step * 10));
        }
    }

    fn verifying(&mut self) {
        (self.on_output)(format!("0x{:06x}: verifying…", self.addr));
    }

    fn finish(&mut self, skipped: bool) {
        if skipped {
            self.addr = self.segments.get(self.index).copied().unwrap_or(self.addr);
        }
        self.index += 1;
        (self.on_output)(if skipped {
            format!("0x{:06x}: unchanged, skipped", self.addr)
        } else {
            format!("0x{:06x}: done", self.addr)
        });
    }
}

pub fn run(
    invocation: &Invocation,
    on_output: impl Fn(String) + Send + Sync,
) -> crate::hooks::Outcome {
    match execute(invocation, &on_output) {
        Ok(()) => {
            on_output(format!("{} finished", invocation.operation.label()));
            crate::hooks::Outcome {
                ok: true,
                text: String::new(),
            }
        }
        Err(text) => crate::hooks::Outcome { ok: false, text },
    }
}

fn execute(invocation: &Invocation, on_output: &impl Fn(String)) -> Result<(), String> {
    let destructive = invocation.operation.destructive();
    on_output(format!("connecting to {}…", invocation.port));
    let serial = serialport::new(&invocation.port, 115_200)
        .flow_control(serialport::FlowControl::None)
        .open_native()
        .map_err(|err| format!("cannot open {}: {err}", invocation.port))?;
    // USB-Serial/JTAG 等需要按 PID 选择复位方式；非 USB 端口用 0 走经典 DTR/RTS 复位。
    let usb = serialport::available_ports()
        .ok()
        .and_then(|ports| {
            ports.into_iter().find_map(|port| match port.port_type {
                serialport::SerialPortType::UsbPort(info) if port.port_name == invocation.port => {
                    Some(info)
                }
                _ => None,
            })
        })
        .unwrap_or(serialport::UsbPortInfo {
            vid: 0,
            pid: 0,
            serial_number: None,
            manufacturer: None,
            product: None,
        });
    let connection = Connection::new(
        serial,
        usb,
        ResetAfterOperation::HardReset,
        ResetBeforeOperation::DefaultReset,
        invocation.baud,
    );
    // 只读查询直接用 ROM，不上传 stub；擦写使用 stub 获得压缩写入与校验。
    let baud = (invocation.baud != 115_200).then_some(invocation.baud);
    let mut flasher = Flasher::connect(connection, destructive, true, true, None, baud)
        .map_err(|err| format!("cannot connect to the ROM bootloader: {err}"))?;
    let chip = flasher.chip();
    let info = flasher
        .device_info()
        .map_err(|err| format!("cannot read device info: {err}"))?;
    let revision = info
        .revision
        .map(|(major, minor)| format!(" (revision v{major}.{minor})"))
        .unwrap_or_default();
    on_output(format!("chip {chip}{revision}"));
    match invocation.operation {
        Operation::ChipId => {
            on_output(format!("crystal {}", info.crystal_frequency));
            on_output(format!("flash size {}", info.flash_size));
            if !info.features.is_empty() {
                on_output(format!("features {}", info.features.join(", ")));
            }
            if let Some(mac) = &info.mac_address {
                on_output(format!("MAC {mac}"));
            }
        }
        Operation::FlashId => on_output(format!("flash size {}", info.flash_size)),
        Operation::ReadMac => on_output(format!(
            "MAC {}",
            info.mac_address.as_deref().unwrap_or("unavailable")
        )),
        Operation::EraseFlash => {
            on_output("erasing entire Flash (this can take a while)…".to_string());
            flasher
                .erase_flash()
                .map_err(|err| format!("erase failed: {err}"))?;
        }
        Operation::WriteFlash | Operation::Flash => {
            write(&mut flasher, &invocation.job, info.flash_size, on_output)?
        }
    }
    flasher
        .connection()
        .reset_after(destructive, chip)
        .map_err(|err| format!("cannot reset the chip: {err}"))?;
    on_output("hard reset".to_string());
    Ok(())
}

fn write(
    flasher: &mut Flasher,
    job: &Job,
    flash_size: espflash::flasher::FlashSize,
    on_output: &impl Fn(String),
) -> Result<(), String> {
    match job {
        Job::Images(images) => {
            let data = images
                .iter()
                .map(|(offset, path)| {
                    std::fs::read(path)
                        .map(|bytes| (*offset, bytes))
                        .map_err(|err| format!("cannot read image {}: {err}", path.display()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let segments: Vec<Segment> = data
                .iter()
                .map(|(offset, bytes)| Segment::new(*offset, bytes))
                .collect();
            write_segments(flasher, &segments, on_output)
        }
        Job::Elf {
            elf,
            bootloader,
            partition_table,
        } => {
            let elf_data = std::fs::read(elf)
                .map_err(|err| format!("cannot read {}: {err}", elf.display()))?;
            let chip = flasher.chip();
            let xtal = chip
                .xtal_frequency(flasher.connection())
                .map_err(|err| format!("cannot read crystal frequency: {err}"))?;
            let flash_data = FlashData::new(
                FlashSettings::new(None, Some(flash_size), None),
                0,
                None,
                chip,
                xtal,
            );
            let image = IdfBootloaderFormat::new(
                &elf_data,
                &flash_data,
                partition_table.as_deref(),
                bootloader.as_deref(),
                None,
                None,
            )
            .map_err(|err| format!("cannot build image from {}: {err}", elf.display()))?;
            // 与 load_image_to_flash 相同的写入流程，但先展开段以便进度输出带上地址。
            let segments = espflash::image_format::ImageFormat::from(image).flash_segments();
            write_segments(flasher, &segments, on_output)
        }
        Job::Query | Job::Erase => Ok(()),
    }
}

fn write_segments(
    flasher: &mut Flasher,
    segments: &[Segment],
    on_output: &impl Fn(String),
) -> Result<(), String> {
    let mut progress = Progress {
        on_output,
        segments: segments.iter().map(|segment| segment.addr).collect(),
        index: 0,
        addr: 0,
        total: 1,
        last_step: 0,
    };
    flasher
        .write_bins_to_flash(segments, &mut progress)
        .map_err(|err| format!("write failed: {err}"))
}

fn parse_offset(value: &str) -> Result<u32, String> {
    let (digits, radix) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or((value, 10), |digits| (digits, 16));
    if digits.is_empty()
        || !digits.bytes().all(|byte| match radix {
            16 => byte.is_ascii_hexdigit(),
            _ => byte.is_ascii_digit(),
        })
    {
        return Err(format!(
            "invalid flash offset {value:?}; use decimal or 0x hexadecimal"
        ));
    }
    u32::from_str_radix(digits, radix)
        .map_err(|_| format!("flash offset {value:?} exceeds the 32-bit address range"))
}

// Quotes group paths; backslashes are always literal, including before a closing quote.
fn split_arguments(arguments: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    for ch in arguments.chars() {
        match quote {
            Some(delimiter) if ch == delimiter => quote = None,
            Some(_) => word.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None => {
                word.push(ch);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("unclosed quote in flash image arguments".into());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Images(std::path::PathBuf);

    impl Images {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "gtui-esptool-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn image(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for Images {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(arguments: &str) -> Result<Invocation, String> {
        build("COM3", 115200, Operation::WriteFlash, arguments)
    }

    #[test]
    fn quote_groups_preserve_windows_backslashes_and_shell_metacharacters() {
        assert_eq!(
            split_arguments(r#"0x1000 "C:\firmware builds\app.bin" 8192 '/tmp/a b.bin'"#).unwrap(),
            [
                "0x1000",
                r"C:\firmware builds\app.bin",
                "8192",
                "/tmp/a b.bin"
            ]
        );
        assert_eq!(
            split_arguments(r#"0 "C:\trailing\""#).unwrap(),
            ["0", "C:\\trailing\\"]
        );
        assert_eq!(
            split_arguments(r#"0 '$(touch owned); & %PATH%.bin'"#).unwrap(),
            ["0", "$(touch owned); & %PATH%.bin"]
        );
        assert!(split_arguments("0 'unclosed").is_err());
    }

    #[test]
    fn image_ranges_use_real_sizes_and_allow_adjacent_pairs() {
        let images = Images::new();
        let first = images.image("first image.bin", &[1; 32]);
        let second = images.image("second image.bin", &[2; 4]);
        assert!(
            write(&format!(
                "0x1000 \"{}\" 4128 \"{}\"",
                first.display(),
                second.display()
            ))
            .is_ok()
        );
        assert!(
            write(&format!(
                "0x1000 \"{}\" 4127 \"{}\"",
                first.display(),
                second.display()
            ))
            .err()
            .unwrap()
            .contains("overlaps")
        );
        assert!(
            write(&format!("0xffffffff \"{}\"", second.display()))
                .err()
                .unwrap()
                .contains("32-bit")
        );
        let one = images.image("last-byte.bin", &[1]);
        assert!(write(&format!("0xffffffff \"{}\"", one.display())).is_ok());
    }

    #[test]
    fn rejects_offsets_flags_missing_and_empty_images() {
        let images = Images::new();
        let image = images.image("valid.bin", &[1]);
        for offset in [
            "-1",
            "+1",
            "0x",
            "0xGG",
            "1e3",
            "4294967296",
            "0x100000000",
            "--help",
            "0;exit",
        ] {
            assert!(
                write(&format!("{offset} \"{}\"", image.display())).is_err(),
                "{offset}"
            );
        }
        for arguments in [
            "",
            "0",
            "0 --force",
            "--force 0",
            "0 missing.bin",
            "0 missing.bin ; touch owned",
        ] {
            assert!(write(arguments).is_err(), "{arguments}");
        }
        let empty = images.image("empty.bin", &[]);
        assert!(
            write(&format!("0 \"{}\"", empty.display()))
                .err()
                .unwrap()
                .contains("nonempty")
        );
        assert!(write(&format!("0 \"{}\"", images.0.display())).is_err());
        for operation in [
            Operation::ChipId,
            Operation::FlashId,
            Operation::ReadMac,
            Operation::EraseFlash,
        ] {
            assert!(build("COM3", 115200, operation, "--force").is_err());
        }
        assert!(build("", 115200, Operation::ReadMac, "").is_err());
        assert!(build("--help", 115200, Operation::ReadMac, "").is_err());
        assert!(build("COM3", 0, Operation::ReadMac, "").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_images_locked_against_reading() {
        use std::os::windows::fs::OpenOptionsExt;
        let images = Images::new();
        let image = images.image("locked.bin", &[1]);
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&image)
            .unwrap();
        let error = write(&format!("0 \"{}\"", image.display())).err().unwrap();
        assert!(
            error.contains("cannot read") || error.contains("cannot inspect"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unreadable_images() {
        use std::os::unix::fs::PermissionsExt;
        let images = Images::new();
        let image = images.image("unreadable.bin", &[1]);
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Privileged users can read mode-000 files; exercise this only when access is denied.
        if File::open(&image).is_err() {
            assert!(
                write(&format!("0 \"{}\"", image.display()))
                    .err()
                    .unwrap()
                    .contains("cannot read")
            );
        }
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn flasher_args_json_drives_one_click_flash() {
        let images = Images::new();
        std::fs::create_dir_all(images.0.join("bootloader")).unwrap();
        images.image("bootloader/bootloader.bin", &[1; 16]);
        images.image("app.bin", &[2; 16]);
        images.image(
            "flasher_args.json",
            br#"{ "write_flash_args": ["--flash-mode", "dio"],
  "flash_files" : { "0x1000" : "bootloader/bootloader.bin", "0x10000" : "app.bin" },
  "app" : { "offset" : "0x10000", "file" : "app.bin" } }"#,
        );
        let invocation = build(
            "COM3",
            460800,
            Operation::Flash,
            &format!("\"{}\"", images.0.display()),
        )
        .unwrap();
        let Job::Images(list) = &invocation.job else {
            panic!("{:?}", invocation.job)
        };
        assert_eq!(
            list.iter().map(|(offset, _)| *offset).collect::<Vec<_>>(),
            [0x1000, 0x10000]
        );
        assert!(list[0].1.ends_with("bootloader/bootloader.bin"));
        assert!(
            invocation.summary.contains("0x010000"),
            "{}",
            invocation.summary
        );
        assert!(Operation::Flash.destructive());
    }

    #[test]
    fn elf_targets_pick_up_build_layout_or_fall_back_to_defaults() {
        let images = Images::new();
        let elf = images.image("app.elf", b"\x7fELF-not-really");
        let invocation = build(
            "COM3",
            460800,
            Operation::Flash,
            &format!("\"{}\"", elf.display()),
        )
        .unwrap();
        assert!(matches!(
            &invocation.job,
            Job::Elf {
                bootloader: None,
                partition_table: None,
                ..
            }
        ));
        assert!(invocation.summary.contains("generated default table"));

        std::fs::create_dir_all(images.0.join("partition_table")).unwrap();
        images.image("partition_table/partition-table.bin", &[0xaa; 32]);
        // A directory without flasher_args.json uses its single ELF.
        let invocation = build(
            "COM3",
            460800,
            Operation::Flash,
            &format!("\"{}\"", images.0.display()),
        )
        .unwrap();
        assert!(matches!(
            &invocation.job,
            Job::Elf {
                bootloader: None,
                partition_table: Some(_),
                ..
            }
        ));

        images.image("other.elf", b"\x7fELF");
        assert!(
            build(
                "COM3",
                460800,
                Operation::Flash,
                &format!("\"{}\"", images.0.display())
            )
            .unwrap_err()
            .contains("several")
        );
        let bin = images.image("raw.bin", &[0; 8]);
        assert!(
            build(
                "COM3",
                460800,
                Operation::Flash,
                &format!("\"{}\"", bin.display())
            )
            .unwrap_err()
            .contains("write-flash")
        );
        assert!(build("COM3", 460800, Operation::Flash, "").is_err());
    }

    #[test]
    fn progress_names_skipped_segments_and_reports_steps() {
        let lines = std::sync::Mutex::new(Vec::new());
        let push = |line: String| lines.lock().unwrap().push(line);
        let mut progress = Progress {
            on_output: &push,
            segments: vec![0x1000, 0x8000, 0x10000],
            index: 0,
            addr: 0,
            total: 1,
            last_step: 0,
        };
        progress.finish(true);
        progress.init(0x8000, 4);
        progress.update(2);
        progress.update(4);
        progress.finish(false);
        progress.finish(true);
        assert_eq!(
            *lines.lock().unwrap(),
            [
                "0x001000: unchanged, skipped",
                "0x008000: writing 4 block(s)…",
                "0x008000: 50%",
                "0x008000: 100%",
                "0x008000: done",
                "0x010000: unchanged, skipped",
            ]
        );
    }

    #[test]
    fn flash_files_parser_handles_idf_output_and_rejects_garbage() {
        assert_eq!(
            flash_files(r#"{"flash_files" : {"0x8000" : "partition_table/partition-table.bin", "65536": "a\\b.bin"}}"#).unwrap(),
            [(0x8000, "partition_table/partition-table.bin".to_string()), (65536, "a\\b.bin".to_string())]
        );
        assert!(flash_files("{}").is_err());
        assert!(flash_files(r#"{"flash_files": {"0x1000"}}"#).is_err());
        assert!(flash_files(r#"{"flash_files": {"zz": "a.bin"}}"#).is_err());
    }
}
