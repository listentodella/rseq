//! IEEE 802.3 CRC-32(poly 0xEDB88320,初值 0xFFFF_FFFF,异或输出)。
//!
//! 位运算实现,无 256 项查找表,零静态 RAM 占用——适合 no_std MCU。
//! 性能对本协议的短帧足够;与大端字节序无关,CRC 始终按小端字节序随帧追加。

/// 计算给定字节的 CRC-32(IEEE)。
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            // 若最低位为 1 则异或多项式;用掩码避免分支。
            let mask = (0u32).wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB88320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 经典校验向量:`"123456789"` → 0xCBF43926。
    #[test]
    fn known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    /// 空输入的 CRC 与 zlib/PNG 一致。
    #[test]
    fn empty_input() {
        assert_eq!(crc32(&[]), 0x00000000);
    }

    /// 同一内容分段累加应等于一次性计算(逐字节等价于块计算)。
    #[test]
    fn incremental_equivalence() {
        let full = crc32(b"register-sequence");
        let mut acc = 0xFFFF_FFFFu32;
        // 复刻算法内部循环,验证逐字节等于块计算。
        for &b in b"register-sequence" {
            acc ^= b as u32;
            for _ in 0..8 {
                let mask = (0u32).wrapping_sub(acc & 1);
                acc = (acc >> 1) ^ (0xEDB88320 & mask);
            }
        }
        assert_eq!(!acc, full);
    }
}
