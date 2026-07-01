use core::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusError {
    InvalidAddress,
    AccessSizeMismatch,
    Timeout,
    HardwareFailure,
}

/// 固定容量栈缓冲，实现 [`core::fmt::Write`]。供 [`Bus::log_vars`] 的默认实现
/// 把 `print!("..{}", v)` 格式化成字符串再交给 [`Bus::log`]，无需 alloc。
struct FmtBuf {
    buf: [u8; 256],
    len: usize,
}

impl FmtBuf {
    fn new() -> Self {
        Self { buf: [0; 256], len: 0 }
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl Write for FmtBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let take = bytes.len().min(256 - self.len);
        self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
        if take < bytes.len() {
            Err(core::fmt::Error)
        } else {
            Ok(())
        }
    }
}

/// 把 `fmt` 中的占位符用 `vals` 的 u32 填充，写入 `w`：
/// - `{}` → 有符号 i32 十进制；
/// - `{x}` → `0x` 十六进制；
/// - `{{` / `}}` → 字面 `{` / `}`；
/// - 占位符多于值时输出 `{?}`。
fn format_vars<W: Write>(w: &mut W, fmt: &str, vals: &[u32]) {
    let mut it = vals.iter();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => match chars.peek().copied() {
                Some('{') => {
                    chars.next();
                    let _ = w.write_char('{');
                }
                Some('}') => {
                    chars.next();
                    match it.next() {
                        Some(v) => {
                            let _ = core::write!(w, "{}", *v as i32);
                        }
                        None => {
                            let _ = w.write_str("{?}");
                        }
                    }
                }
                Some('x') => {
                    chars.next();
                    if chars.peek().copied() == Some('}') {
                        chars.next();
                        match it.next() {
                            Some(v) => {
                                let _ = core::write!(w, "0x{:x}", *v);
                            }
                            None => {
                                let _ = w.write_str("{?}");
                            }
                        }
                    } else {
                        let _ = w.write_str("{x");
                    }
                }
                _ => {
                    let _ = w.write_char('{');
                }
            },
            '}' => {
                if chars.peek().copied() == Some('}') {
                    chars.next();
                    let _ = w.write_char('}');
                } else {
                    let _ = w.write_char('}');
                }
            }
            _ => {
                let _ = w.write_char(c);
            }
        }
    }
}

/// 总线操作 trait，MCU 侧需要实现这个 trait
pub trait Bus {
    /// 从总线读取 n 个字节
    fn read(&mut self, addr: u32, data: &mut [u8]) -> Result<(), BusError>;

    /// 向总线写入 n 个字节
    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BusError>;

    /// 延迟微秒
    fn delay_us(&mut self, us: u32) -> Result<(), BusError>;

    /// 调试打印（`print!` 语句）。默认 no-op：不关心日志的总线实现可不变。
    /// 关心的实现（`TracingBus` 回传 trace、`MockBus` 记录、MCU `ImuSpiBus` 走
    /// printk）覆盖此方法即可。
    fn log(&mut self, _msg: &str) -> Result<(), BusError> {
        Ok(())
    }

    /// 带变量插值的打印（`print!("..{}", v)`）。默认实现把 `fmt` 与 `vals`
    /// 格式化进栈缓冲后委托给 [`Bus::log`]——所以各实现无需单独覆盖：
    /// `TracingBus`/`MockBus`/`ImuSpiBus` 经由各自的 `log` 即可让插值打印
    /// 同时落到 USART3 与主机 trace 流。
    fn log_vars(&mut self, fmt: &str, vals: &[u32]) -> Result<(), BusError> {
        let mut buf = FmtBuf::new();
        format_vars(&mut buf, fmt, vals);
        self.log(buf.as_str())
    }
}
