//! Chip register dictionary loaded from YAML descriptions.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChipError {
    Io(String),
    Parse(String),
    NotFound(String),
    AmbiguousRegister { name: String, pages: Vec<String> },
    FieldNotFound(String),
    EventNotFound(String),
    FieldValueOutOfRange { field: String, value: u32, max: u32 },
    RegisterNotUpdatable { name: String, access: String },
    InvalidUpdate(String),
}

impl std::fmt::Display for ChipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "IO error: {msg}"),
            Self::Parse(msg) => write!(f, "YAML parse error: {msg}"),
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
            Self::AmbiguousRegister { name, pages } => {
                write!(
                    f,
                    "ambiguous register '{name}', found in pages: {}",
                    pages.join(", ")
                )
            }
            Self::FieldNotFound(msg) => write!(f, "field not found: {msg}"),
            Self::EventNotFound(msg) => write!(f, "interrupt event not found: {msg}"),
            Self::FieldValueOutOfRange { field, value, max } => {
                write!(
                    f,
                    "value {value} out of range for field '{field}' (max {max})"
                )
            }
            Self::RegisterNotUpdatable { name, access } => {
                write!(
                    f,
                    "register '{name}' access={access} cannot be updated (requires RW)"
                )
            }
            Self::InvalidUpdate(msg) => write!(f, "invalid update: {msg}"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ChipYaml {
    sensor: String,
    #[serde(default)]
    who_am_i: Option<WhoAmIYaml>,
    #[serde(default)]
    controls: Vec<ControlYaml>,
    pages: HashMap<String, PageYaml>,
}

#[derive(Debug, Clone, Deserialize)]
struct WhoAmIYaml {
    #[serde(deserialize_with = "deserialize_u32")]
    reg: u32,
    #[serde(default)]
    values: Vec<WhoAmIValueYaml>,
}

#[derive(Debug, Clone, Deserialize)]
struct WhoAmIValueYaml {
    #[serde(deserialize_with = "deserialize_u32")]
    value: u32,
    #[serde(default)]
    desc: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PageYaml {
    #[serde(default, deserialize_with = "deserialize_u32_option")]
    page_id: Option<u32>,
    #[serde(default)]
    access: String,
    #[serde(default)]
    desc: String,
    registers: Vec<RegisterYaml>,
}

#[derive(Debug, Clone, Deserialize)]
struct RegisterYaml {
    #[serde(deserialize_with = "deserialize_u32")]
    addr: u32,
    name: String,
    #[serde(default)]
    access: String,
    #[serde(default, deserialize_with = "deserialize_u32_option")]
    width: Option<u32>,
    #[serde(default)]
    desc: String,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    read_clear: bool,
    #[serde(default)]
    no_dump: bool,
    #[serde(default)]
    no_dump_reason: String,
    #[serde(default)]
    fields: Vec<FieldYaml>,
}

#[derive(Debug, Clone, Deserialize)]
struct FieldYaml {
    name: String,
    bits: String,
    #[serde(default)]
    desc: String,
    #[serde(default)]
    event: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ControlYaml {
    name: String,
    target: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    desc: String,
    /// Optional host-side report decoder property adjusted by this control.
    /// Currently understood by host tools for runtime full-scale changes.
    #[serde(default)]
    report_scale: Option<String>,
    #[serde(default)]
    options: Vec<ControlOptionYaml>,
}

#[derive(Debug, Clone, Deserialize)]
struct ControlOptionYaml {
    #[serde(deserialize_with = "deserialize_u32")]
    value: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    desc: String,
    /// Physical full-scale value associated with this register encoding.
    #[serde(default)]
    scale: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhoAmI {
    pub reg: u32,
    pub values: Vec<(u32, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub name: String,
    pub page_id: Option<u32>,
    pub access: String,
    pub desc: String,
    pub registers: Vec<Register>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub bit_hi: u8,
    pub bit_lo: u8,
    pub desc: String,
    /// 当该位属于某个中断状态/使能位时，对应芯片字典里声明的事件名。
    pub event: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Register {
    pub addr: u32,
    pub name: String,
    pub access: String,
    pub width: u32,
    pub desc: String,
    /// 寄存器的语义角色（如 interrupt_status / interrupt_status_snapshot）。
    pub roles: Vec<String>,
    /// 读取后硬件自动清零（W1C-on-read 中断状态寄存器）。
    pub read_clear: bool,
    /// 不适合普通 register dump 读取的寄存器。
    pub no_dump: bool,
    /// `no_dump` 的芯片字典说明。
    pub no_dump_reason: String,
    pub fields: Vec<Field>,
}

/// 一个用户可调参数，通常映射到某个寄存器位域，例如输出速率、滤波器或量程。
#[derive(Debug, Clone, PartialEq)]
pub struct Control {
    pub name: String,
    pub target: String,
    pub group: String,
    pub desc: String,
    pub report_scale: Option<String>,
    pub options: Vec<ControlOption>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ControlOption {
    pub value: u32,
    pub name: String,
    pub label: String,
    pub desc: String,
    pub scale: Option<f64>,
}

/// 一个中断事件在状态寄存器中的位置，供 irq! 派发表解析。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBit {
    /// 该事件所在独立状态寄存器的地址。
    pub status_addr: u32,
    pub bit_lo: u8,
    pub bit_hi: u8,
    /// 该状态寄存器是否读取后清零。
    pub read_clear: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldUpdate {
    pub bit_lo: u8,
    pub bit_hi: u8,
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePlan {
    pub addr: u32,
    pub width: u32,
    pub register_name: String,
    pub fields: Vec<FieldUpdate>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    pub sensor: String,
    pub who_am_i: Option<WhoAmI>,
    pub controls: Vec<Control>,
    pub pages: Vec<Page>,
    pub source: PathBuf,
}

#[derive(Debug, Default, Clone)]
pub struct ChipRegistry {
    chips: Vec<Chip>,
    by_page_and_name: HashMap<(String, String), RegisterRef>,
    by_name: HashMap<String, Vec<RegisterRef>>,
    by_page_reg_field: HashMap<(String, String, String), FieldRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegisterRef {
    chip: usize,
    page: String,
    register: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FieldRef {
    chip: usize,
    page: String,
    register: usize,
    field: usize,
}

impl ChipRegistry {
    pub fn load(path: &Path) -> Result<Self, ChipError> {
        let mut registry = Self::default();
        registry.load_file(path)?;
        Ok(registry)
    }

    pub fn load_file(&mut self, path: &Path) -> Result<(), ChipError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ChipError::Io(format!("{}: {e}", path.display())))?;
        let yaml: ChipYaml = serde_yaml::from_str(&content)
            .map_err(|e| ChipError::Parse(format!("{}: {e}", path.display())))?;

        let chip_idx = self.chips.len();
        let mut pages = Vec::new();

        for (page_name, page_yaml) in yaml.pages {
            let registers = page_yaml
                .registers
                .into_iter()
                .map(|reg| {
                    let fields = reg
                        .fields
                        .into_iter()
                        .map(|f| {
                            let (bit_hi, bit_lo) = parse_bits(&f.bits).map_err(|e| {
                                ChipError::Parse(format!(
                                    "{}.{}.{}: {e}",
                                    page_name, reg.name, f.name
                                ))
                            })?;
                            Ok(Field {
                                name: f.name,
                                bit_hi,
                                bit_lo,
                                desc: f.desc,
                                event: f.event,
                            })
                        })
                        .collect::<Result<Vec<_>, ChipError>>()?;

                    Ok(Register {
                        addr: reg.addr,
                        name: reg.name.clone(),
                        access: reg.access,
                        width: reg.width.unwrap_or(1),
                        desc: reg.desc,
                        roles: reg.roles,
                        read_clear: reg.read_clear,
                        no_dump: reg.no_dump,
                        no_dump_reason: reg.no_dump_reason,
                        fields,
                    })
                })
                .collect::<Result<Vec<_>, ChipError>>()?;

            for (reg_idx, reg) in registers.iter().enumerate() {
                let key = (page_name.clone(), reg.name.clone());
                self.by_page_and_name.insert(
                    key,
                    RegisterRef {
                        chip: chip_idx,
                        page: page_name.clone(),
                        register: reg_idx,
                    },
                );
                self.by_name
                    .entry(reg.name.clone())
                    .or_default()
                    .push(RegisterRef {
                        chip: chip_idx,
                        page: page_name.clone(),
                        register: reg_idx,
                    });

                for (field_idx, field) in reg.fields.iter().enumerate() {
                    self.by_page_reg_field.insert(
                        (page_name.clone(), reg.name.clone(), field.name.clone()),
                        FieldRef {
                            chip: chip_idx,
                            page: page_name.clone(),
                            register: reg_idx,
                            field: field_idx,
                        },
                    );
                }
            }

            pages.push(Page {
                name: page_name,
                page_id: page_yaml.page_id,
                access: page_yaml.access,
                desc: page_yaml.desc,
                registers,
            });
        }

        pages.sort_by(|a, b| a.name.cmp(&b.name));

        let who_am_i = yaml.who_am_i.map(|w| WhoAmI {
            reg: w.reg,
            values: w.values.into_iter().map(|v| (v.value, v.desc)).collect(),
        });
        let controls = yaml
            .controls
            .into_iter()
            .map(|control| Control {
                name: control.name,
                target: control.target,
                group: control.group,
                desc: control.desc,
                report_scale: control.report_scale,
                options: control
                    .options
                    .into_iter()
                    .map(|option| ControlOption {
                        value: option.value,
                        name: option.name,
                        label: option.label,
                        desc: option.desc,
                        scale: option.scale,
                    })
                    .collect(),
            })
            .collect();

        self.chips.push(Chip {
            sensor: yaml.sensor,
            who_am_i,
            controls,
            pages,
            source: path.to_path_buf(),
        });

        Ok(())
    }

    pub fn chips(&self) -> &[Chip] {
        &self.chips
    }

    pub fn resolve_register(&self, name: &str) -> Result<(u32, &Register), ChipError> {
        if let Some((page, reg_name)) = name.split_once('.') {
            return self.resolve_in_page(page, reg_name);
        }

        let refs = self
            .by_name
            .get(name)
            .ok_or_else(|| ChipError::NotFound(format!("register '{name}'")))?;

        if refs.len() > 1 {
            let pages = refs.iter().map(|r| r.page.clone()).collect();
            return Err(ChipError::AmbiguousRegister {
                name: name.to_string(),
                pages,
            });
        }

        let reg_ref = &refs[0];
        let register =
            &self.chips[reg_ref.chip].pages[self.page_index(reg_ref)?].registers[reg_ref.register];
        Ok((register.addr, register))
    }

    pub fn resolve_in_page(&self, page: &str, name: &str) -> Result<(u32, &Register), ChipError> {
        let reg_ref = self
            .by_page_and_name
            .get(&(page.to_string(), name.to_string()))
            .ok_or_else(|| ChipError::NotFound(format!("register '{page}.{name}'")))?;

        let register =
            &self.chips[reg_ref.chip].pages[self.page_index(reg_ref)?].registers[reg_ref.register];
        Ok((register.addr, register))
    }

    fn page_index(&self, reg_ref: &RegisterRef) -> Result<usize, ChipError> {
        self.chips[reg_ref.chip]
            .pages
            .iter()
            .position(|p| p.name == reg_ref.page)
            .ok_or_else(|| ChipError::NotFound(format!("page '{}'", reg_ref.page)))
    }

    fn register_at<'a>(&'a self, reg_ref: &RegisterRef) -> Result<&'a Register, ChipError> {
        Ok(&self.chips[reg_ref.chip].pages[self.page_index(reg_ref)?].registers[reg_ref.register])
    }

    fn field_at<'a>(&'a self, field_ref: &FieldRef) -> Result<&'a Field, ChipError> {
        Ok(&self
            .register_at(&RegisterRef {
                chip: field_ref.chip,
                page: field_ref.page.clone(),
                register: field_ref.register,
            })?
            .fields[field_ref.field])
    }

    fn ensure_updatable(register: &Register) -> Result<(), ChipError> {
        if register.access != "RW" {
            return Err(ChipError::RegisterNotUpdatable {
                name: register.name.clone(),
                access: register.access.clone(),
            });
        }
        Ok(())
    }

    fn field_update(field: &Field, value: u32) -> Result<FieldUpdate, ChipError> {
        let width = (field.bit_hi - field.bit_lo + 1) as u32;
        let max = if width >= 32 {
            u32::MAX
        } else {
            (1u32 << width) - 1
        };
        if value > max {
            return Err(ChipError::FieldValueOutOfRange {
                field: field.name.clone(),
                value,
                max,
            });
        }
        Ok(FieldUpdate {
            bit_lo: field.bit_lo,
            bit_hi: field.bit_hi,
            value,
        })
    }

    pub fn plan_update(
        &self,
        target: &str,
        updates: &[(String, u32)],
    ) -> Result<UpdatePlan, ChipError> {
        let parts: Vec<&str> = target.split('.').collect();
        match parts.as_slice() {
            [page, reg, field] if updates.len() == 1 && updates[0].0 == *field => {
                let register = self.register_at(
                    self.by_page_and_name
                        .get(&(page.to_string(), reg.to_string()))
                        .ok_or_else(|| ChipError::NotFound(format!("register '{page}.{reg}'")))?,
                )?;
                Self::ensure_updatable(register)?;
                let field = self.field_at(
                    self.by_page_reg_field
                        .get(&(page.to_string(), reg.to_string(), field.to_string()))
                        .ok_or_else(|| ChipError::FieldNotFound(format!("{page}.{reg}.{field}")))?,
                )?;
                Ok(UpdatePlan {
                    addr: register.addr,
                    width: register.width,
                    register_name: format!("{page}.{reg}"),
                    fields: vec![Self::field_update(field, updates[0].1)?],
                })
            }
            [page, reg] => {
                let register = self.register_at(
                    self.by_page_and_name
                        .get(&(page.to_string(), reg.to_string()))
                        .ok_or_else(|| ChipError::NotFound(format!("register '{page}.{reg}'")))?,
                )?;
                Self::ensure_updatable(register)?;
                let mut fields = Vec::with_capacity(updates.len());
                for (field_name, value) in updates {
                    let field = self.field_at(
                        self.by_page_reg_field
                            .get(&(page.to_string(), reg.to_string(), field_name.clone()))
                            .ok_or_else(|| {
                                ChipError::FieldNotFound(format!("{page}.{reg}.{field_name}"))
                            })?,
                    )?;
                    fields.push(Self::field_update(field, *value)?);
                }
                Ok(UpdatePlan {
                    addr: register.addr,
                    width: register.width,
                    register_name: format!("{page}.{reg}"),
                    fields,
                })
            }
            _ => Err(ChipError::InvalidUpdate(format!(
                "expected PAGE.REG.FIELD with one value, or PAGE.REG with {{ field: value, ... }}, got '{target}'"
            ))),
        }
    }

    /// 遍历所有已加载芯片的寄存器。
    fn registers(&self) -> impl Iterator<Item = &Register> {
        self.chips
            .iter()
            .flat_map(|chip| chip.pages.iter())
            .flat_map(|page| page.registers.iter())
    }

    /// 查找声明了 `interrupt_status_snapshot` 角色的寄存器，
    /// 它是一次性读取整组中断状态的"快照视图"。
    /// 返回 (起始地址, 字节宽度, 读后是否清零)。
    pub fn interrupt_snapshot(&self) -> Option<(u32, u32, bool)> {
        self.registers()
            .find(|reg| {
                reg.roles
                    .iter()
                    .any(|role| role == "interrupt_status_snapshot")
            })
            .map(|reg| (reg.addr, reg.width, reg.read_clear))
    }

    /// 把一个中断事件名解析为它在独立状态寄存器中的位位置。
    /// 只在带有 `interrupt_status` 角色的寄存器里查找（排除快照视图，
    /// 否则同一事件会与快照里同地址的位重复匹配）。
    pub fn resolve_event(&self, event: &str) -> Result<EventBit, ChipError> {
        for reg in self.registers() {
            let is_status = reg.roles.iter().any(|role| role == "interrupt_status");
            if !is_status {
                continue;
            }
            for field in &reg.fields {
                if field.event.as_deref() == Some(event) {
                    return Ok(EventBit {
                        status_addr: reg.addr,
                        bit_lo: field.bit_lo,
                        bit_hi: field.bit_hi,
                        read_clear: reg.read_clear,
                    });
                }
            }
        }
        Err(ChipError::EventNotFound(event.to_string()))
    }
}

pub fn normalize_chip_path(path: &str) -> String {
    if path.ends_with(".yaml") || path.ends_with(".yml") {
        path.to_string()
    } else {
        format!("{path}.yaml")
    }
}

pub fn resolve_chip_path(path: &str, base_dir: Option<&Path>) -> PathBuf {
    let normalized = normalize_chip_path(path);
    let candidate = PathBuf::from(&normalized);
    if candidate.is_absolute() {
        return candidate;
    }

    if let Some(base) = base_dir {
        let relative_to_base = base.join(&normalized);
        if relative_to_base.exists() {
            return relative_to_base;
        }
    }

    if candidate.exists() {
        return candidate;
    }

    candidate
}

pub fn load_chip(path: &Path) -> Result<Chip, ChipError> {
    ChipRegistry::load(path)?
        .chips()
        .first()
        .cloned()
        .ok_or_else(|| ChipError::Parse(format!("no chip definition found in {}", path.display())))
}

fn deserialize_u32<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Unexpected, Visitor};

    struct U32Visitor;

    impl Visitor<'_> for U32Visitor {
        type Value = u32;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("an integer or hex string")
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            u32::try_from(value).map_err(|_| E::invalid_value(Unexpected::Unsigned(value), &self))
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            u32::try_from(value).map_err(|_| E::invalid_value(Unexpected::Signed(value), &self))
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_u32_text(value).map_err(E::custom)
        }
    }

    deserializer.deserialize_any(U32Visitor)
}

fn deserialize_u32_option<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_u32(deserializer).map(Some)
}

fn parse_bits(bits: &str) -> Result<(u8, u8), String> {
    let parts: Vec<&str> = bits.split(':').collect();
    match parts.as_slice() {
        [hi, lo] => {
            let bit_hi: u8 = hi
                .trim()
                .parse()
                .map_err(|e| format!("invalid bit hi: {e}"))?;
            let bit_lo: u8 = lo
                .trim()
                .parse()
                .map_err(|e| format!("invalid bit lo: {e}"))?;
            if bit_hi < bit_lo {
                return Err(format!("bit range '{bits}' has hi < lo"));
            }
            Ok((bit_hi, bit_lo))
        }
        _ => Err(format!("invalid bit range '{bits}', expected 'hi:lo'")),
    }
}

pub fn emit_update_bytecode(bytecode: &mut Vec<u8>, plan: &UpdatePlan, delay_us: u32) {
    bytecode.push(rseq_vm::Opcode::Update as u8);
    bytecode.extend_from_slice(&plan.addr.to_le_bytes());
    bytecode.extend_from_slice(&plan.width.to_le_bytes());
    bytecode.extend_from_slice(&delay_us.to_le_bytes());
    bytecode.extend_from_slice(&(plan.fields.len() as u32).to_le_bytes());
    for field in &plan.fields {
        bytecode.push(field.bit_lo);
        bytecode.push(field.bit_hi);
        bytecode.extend_from_slice(&field.value.to_le_bytes());
    }
}

/// Construct a register's raw bytes from field updates — the compile-time
/// equivalent of `update!`'s read-modify-write, but with NO read: bits not
/// covered by any listed field are 0, so this is a deterministic whole-byte
/// set built purely from the field values. Used by `write!(REG, { field: v })`.
pub fn fields_to_bytes(width: u32, fields: &[FieldUpdate]) -> Vec<u8> {
    let mut buf = vec![0u8; width as usize];
    for fu in fields {
        let nbits = (fu.bit_hi - fu.bit_lo + 1) as usize;
        for bit in 0..nbits {
            let abs = fu.bit_lo as usize + bit;
            let byte_idx = abs / 8;
            let bit_idx = abs % 8;
            if byte_idx < buf.len() && (fu.value >> bit) & 1 == 1 {
                buf[byte_idx] |= 1u8 << bit_idx;
            }
        }
    }
    buf
}

fn parse_u32_text(text: &str) -> Result<u32, String> {
    let trimmed = text.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).map_err(|e| format!("invalid hex '{text}': {e}"))
    } else {
        trimmed
            .parse::<u32>()
            .map_err(|e| format!("invalid integer '{text}': {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_qmi8660_yaml() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let registry = ChipRegistry::load(&path).expect("load qmi8660.yaml");

        assert_eq!(registry.chips().len(), 1);
        let chip = &registry.chips()[0];
        assert_eq!(chip.sensor, "QMI8660");
        assert!(chip.pages.iter().any(|p| p.name == "UI"));
        assert!(chip.pages.iter().any(|p| p.name == "OIS"));

        let (addr, reg) = registry.resolve_in_page("UI", "WHOAMI").unwrap();
        assert_eq!(addr, 0x02);
        assert_eq!(reg.name, "WHOAMI");

        let err = registry.resolve_register("WHOAMI").unwrap_err();
        assert!(matches!(err, ChipError::AmbiguousRegister { .. }));

        let (addr, reg) = registry.resolve_register("UI.WHOAMI").unwrap();
        assert_eq!(addr, 0x02);
        assert_eq!(reg.name, "WHOAMI");

        let (addr, reg) = registry.resolve_register("UI.FIFO_DATA").unwrap();
        assert_eq!(addr, 0x57);
        assert!(reg.no_dump);
        assert!(!reg.no_dump_reason.is_empty());
    }

    #[test]
    fn plan_single_field_update() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let registry = ChipRegistry::load(&path).unwrap();
        let plan = registry
            .plan_update(
                "UI.COMM_CTL.sda_scl_pu_dis",
                &[("sda_scl_pu_dis".into(), 1)],
            )
            .unwrap();
        assert_eq!(plan.addr, 0x0B);
        assert_eq!(plan.fields.len(), 1);
        assert_eq!(plan.fields[0].bit_lo, 1);
        assert_eq!(plan.fields[0].bit_hi, 1);
    }

    #[test]
    fn plan_multi_field_update() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let registry = ChipRegistry::load(&path).unwrap();
        let plan = registry
            .plan_update(
                "UI.COMM_CTL",
                &[("cs_pu_dis".into(), 1), ("sda_scl_pu_dis".into(), 0)],
            )
            .unwrap();
        assert_eq!(plan.fields.len(), 2);
    }
}
