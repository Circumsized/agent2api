//! Devin 上游的 **protobuf 手写编解码** 与 **Connect 帧**（RPC 传输信封）。
//!
//! ── 为什么手写而不是引入 protobuf 依赖 ────────────────────────
//! Devin 的推理走 Windsurf 的 Connect-RPC（`GetChatMessage`），报文是 protobuf。
//! 本仓的 `Cargo.toml` **没有** protobuf / tonic / prost 任何一个依赖（全仓其它家
//! 的上游要么是 JSON、要么是自有信封），为一家新增一整套 protobuf 代码生成链
//! （prost-build + protoc）不成比例。Devin 用到的字段屈指可数、且参考实现
//! （magpie-community/plugins 的 `packages/devin/index.mjs`，MIT）本身就是手写
//! 的 writer / reader —— 这里逐字移植同一套最小实现，语义与源实现一一对应，
//! 出问题时也能对着源实现逐行对拍。
//!
//! ── 传输信封（Connect 帧）─────────────────────────────────────
//! 请求体是**一个** Connect 帧：5 字节前缀 + 载荷。
//!   字节 0：标志位（bit0 = gzip，bit1 = 流结束）；字节 1..4：载荷长度的**大端** u32。
//! 响应体是**一串**这样的帧：普通帧是载荷帧，最后一帧置 bit1 并携带一个 JSON
//! 错误对象（成功时是一个空对象 `{}`）。所以解码要能边收边切帧
//! （[`FrameDecoder`]），而不是「读满 5 字节再读满一整帧」。
//!
//! gzip 用仓里已有的 `flate2`（Qoder 的 UMID 组件解压已在用它，不是新依赖）。

use std::io::Read;

/// Connect 帧标志：bit0 = 载荷被 gzip 压缩
pub const FLAG_GZIP: u8 = 1;
/// Connect 帧标志：bit1 = 这是流结束帧（它的载荷是一个 JSON 结果 / 错误对象）
pub const FLAG_END: u8 = 2;
/// 单帧载荷的硬上限（64 MiB）：畸形流据此**当场**止步，而不是无限吃内存
const MAX_FRAME_BYTES: usize = 64 << 20;

/// 逐字段构造一个 protobuf 消息。
///
/// 与参考实现的 `PB` 类同名同义：`uvarint` 写裸变长整数，其余方法各写一个
/// 「字段号 + 线类型」的键再写值。`done()` 交出完整字节。
#[derive(Clone, Default)]
pub struct Pb {
    buf: Vec<u8>,
}

impl Pb {
    pub fn new() -> Self {
        Self::default()
    }

    /// 变长无符号整数（protobuf 的 base-128，小端 7 位一组）
    pub fn uvarint(&mut self, mut n: u64) {
        loop {
            if n < 0x80 {
                self.buf.push(n as u8);
                return;
            }
            self.buf.push(((n as u8) & 0x7f) | 0x80);
            n >>= 7;
        }
    }

    /// 字段号 + 线类型 0（varint）
    pub fn varint(&mut self, field: u32, v: u64) -> &mut Self {
        self.uvarint((u64::from(field) << 3) | 0);
        self.uvarint(v);
        self
    }

    /// 字段号 + 线类型 2（长度前缀字节串）——字符串与嵌套消息都走它
    pub fn bytes(&mut self, field: u32, v: &[u8]) -> &mut Self {
        self.uvarint((u64::from(field) << 3) | 2);
        self.uvarint(v.len() as u64);
        self.buf.extend_from_slice(v);
        self
    }

    /// 同上，值取自 `&str`
    pub fn str(&mut self, field: u32, v: &str) -> &mut Self {
        self.bytes(field, v.as_bytes())
    }

    /// 字段号 + 线类型 1（固定 64 位）——Devin 的 temperature / top_p 是 double
    pub fn double(&mut self, field: u32, v: f64) -> &mut Self {
        self.uvarint((u64::from(field) << 3) | 1);
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// 字段号 + 线类型 5（固定 32 位）——模型价格（`ClientModelConfig` 的 32）是
    /// float32。写给读侧（[`float32`]）同一字节序（小端）。
    pub fn float(&mut self, field: u32, v: f32) -> &mut Self {
        self.uvarint((u64::from(field) << 3) | 5);
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// 交出当前字节（不消耗，可继续追加）
    pub fn done(&self) -> Vec<u8> {
        self.buf.clone()
    }

    /// 交出字节（消耗）
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// 一个已解析的 protobuf 字段。
///
/// `wire` 与 `n` / `data` 的取值与参考实现的 `fields()` 一致：
///   - wire 0（varint）：值在 `n`；
///   - wire 1（fixed64）：载荷在 `data`（8 字节；双精度温度 / top_p 就是它）；
///   - wire 2（长度前缀）：载荷在 `data`；
///   - wire 5（fixed32）：4 字节载荷在 `data`（价格就是它）。
#[derive(Clone, Debug, Default)]
pub struct Field {
    pub num: u32,
    pub wire: u32,
    pub n: u64,
    pub data: Vec<u8>,
}

/// `data` 的字段是否存在且非空
impl Field {
    pub fn has_data(&self) -> bool {
        !self.data.is_empty()
    }
}

/// 读一个消息的全部字段；**畸形尾巴直接丢掉**（与参考实现一致：宁缺勿弃整包）。
pub fn fields(bytes: &[u8]) -> Vec<Field> {
    let mut out = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let Some(key) = read_uvarint(bytes, &mut index) else {
            break;
        };
        let field = Field {
            num: (key >> 3) as u32,
            wire: (key & 7) as u32,
            n: 0,
            data: Vec::new(),
        };
        let mut field = field;
        match field.wire {
            0 => {
                let Some(value) = read_uvarint(bytes, &mut index) else {
                    break;
                };
                field.n = value;
            }
            1 => {
                if index + 8 > bytes.len() {
                    break;
                }
                field.data = bytes[index..index + 8].to_vec();
                index += 8;
            }
            2 => {
                let Some(length) = read_uvarint(bytes, &mut index) else {
                    break;
                };
                let length = length as usize;
                if index + length > bytes.len() {
                    break;
                }
                field.data = bytes[index..index + length].to_vec();
                index += length;
            }
            5 => {
                if index + 4 > bytes.len() {
                    break;
                }
                field.data = bytes[index..index + 4].to_vec();
                index += 4;
            }
            // 线类型 3 / 4（已废弃的 group）与 6 / 7：认不出就止步
            _ => break,
        }
        out.push(field);
    }
    out
}

/// 读一个变长无符号整数；越界或超过 10 字节视为无效（返回 None）。
fn read_uvarint(bytes: &[u8], index: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*index)?;
        *index += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

/// 线类型 5 的四字节小端 float32（价格字段）。
///
/// 参考实现用 `getFloat32(0, true)`（小端）——与 `Pb::double` 的
/// `setFloat64(0, v, true)` 同一字节序。
pub fn float32(data: &[u8]) -> Option<f32> {
    let bytes: [u8; 4] = data.get(0..4)?.try_into().ok()?;
    Some(f32::from_le_bytes(bytes))
}

/// 把一个消息包成 **Connect 流的一个帧**。
///
/// 请求侧只有一个帧（标志位 0），长度是大端 u32。
pub fn frame(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + message.len());
    out.push(0);
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// 一个已解码的 Connect 帧。
#[derive(Clone, Debug)]
pub struct Frame {
    /// 是否流结束帧（其载荷是 JSON 结果 / 错误对象）
    pub end: bool,
    /// 解压后的载荷字节
    pub data: Vec<u8>,
}

/// 边收边切 Connect 帧的解码器（响应体是字节流，帧不一定按边界到达）。
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一批新字节，返回这次能切出的全部完整帧。
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, String> {
        self.buf.extend_from_slice(chunk);
        self.drain()
    }

    /// 流结束时校验：还有半个帧没拼齐就是「意外 EOF」。
    pub fn finish(&self) -> Result<(), String> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err("上游流提前结束（有半帧没拼齐）".to_string())
        }
    }

    fn drain(&mut self) -> Result<Vec<Frame>, String> {
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 5 {
                break;
            }
            let length = u32::from_be_bytes([
                self.buf[1],
                self.buf[2],
                self.buf[3],
                self.buf[4],
            ]) as usize;
            if length > MAX_FRAME_BYTES {
                return Err("上游帧长度非法（疑似畸形流）".to_string());
            }
            if self.buf.len() < 5 + length {
                break;
            }
            let flags = self.buf[0];
            let mut data = self.buf[5..5 + length].to_vec();
            self.buf.drain(..5 + length);
            if flags & FLAG_GZIP != 0 {
                data = gunzip(&data)?;
            }
            out.push(Frame {
                end: flags & FLAG_END != 0,
                data,
            });
        }
        Ok(out)
    }
}

/// gzip 解压一个帧载荷（Connect 允许逐帧压缩）。
fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = flate2::read::GzDecoder::new(data);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|error| format!("Connect 帧 gzip 解压失败: {error}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    //! 这一组钉的是「字节对不对」——所有上层解析（模型目录 / 回复流）都建在它上面，
    //! 这里错一个字节，上面的表现会是「模型列表少几条」这种远端的静默故障。
    use super::*;

    #[test]
    fn varint_and_bytes_round_trip_through_the_reader() {
        let mut pb = Pb::new();
        pb.varint(1, 1).varint(2, 128_000).double(5, 0.7);
        let nested = {
            let mut inner = Pb::new();
            inner.varint(1, 1).varint(2, 4096).varint(3, 400).str(7, "x");
            inner.done()
        };
        pb.bytes(8, &nested).str(21, "swe-2-high");
        let bytes = pb.done();

        let parsed = fields(&bytes);
        // field 1: varint 1
        assert_eq!((1, 0, 1u64), (parsed[0].num, parsed[0].wire, parsed[0].n));
        // field 2: varint 128000（越过 0x80 边界，验证多字节变长）
        assert_eq!((2, 0, 128_000u64), (parsed[1].num, parsed[1].wire, parsed[1].n));
        // field 5: fixed64 double 0.7
        assert_eq!(5, parsed[2].num);
        assert_eq!(1, parsed[2].wire);
        let double = f64::from_le_bytes(parsed[2].data.as_slice().try_into().unwrap());
        assert!((double - 0.7).abs() < 1e-9);
        // field 8: 嵌套消息
        assert_eq!(8, parsed[3].num);
        let inner = fields(&parsed[3].data);
        assert_eq!(1, inner[0].n);
        assert_eq!(4096, inner[1].n);
        assert_eq!(400, inner[2].n);
        assert_eq!("x", String::from_utf8(inner[3].data.clone()).unwrap());
        // field 21: 字符串
        assert_eq!("swe-2-high", String::from_utf8(parsed[4].data.clone()).unwrap());
    }

    #[test]
    fn a_malformed_tail_is_dropped_not_fatal() {
        let mut pb = Pb::new();
        pb.str(1, "ok");
        let mut bytes = pb.done();
        // 尾部只给一个「长度说要 8 字节、实际只剩 2 字节」的半条字段
        bytes.push((2 << 3) | 2);
        bytes.push(8);
        bytes.push(b'a');
        bytes.push(b'b');
        let parsed = fields(&bytes);
        // 前面那条完整字段留着，半条丢掉 —— 整包不作废
        assert_eq!(1, parsed.len());
        assert_eq!("ok", String::from_utf8(parsed[0].data.clone()).unwrap());
    }

    #[test]
    fn fixed32_float_is_little_endian() {
        let mut pb = Pb::new();
        pb.uvarint((2 << 3) | 5);
        pb.buf.extend_from_slice(&1.5f32.to_le_bytes());
        let bytes = pb.done();
        let parsed = fields(&bytes);
        assert_eq!(Some(1.5), float32(&parsed[0].data));
    }

    #[test]
    fn frames_split_over_chunk_boundaries_and_gzip_is_handled() {
        let payload = b"hello protobuf";
        let encoded = frame(payload);
        let mut decoder = FrameDecoder::new();
        // 拆成三段喂：前缀 / 载荷前半 / 载荷后半
        assert!(decoder.push(&encoded[..3]).unwrap().is_empty());
        assert!(decoder.push(&encoded[3..7]).unwrap().is_empty());
        let frames = decoder.push(&encoded[7..]).unwrap();
        assert_eq!(1, frames.len());
        assert!(!frames[0].end);
        assert_eq!(payload, frames[0].data.as_slice());
        decoder.finish().unwrap();

        // 两帧连在一起（一个普通帧 + 一个 end 帧）
        let mut stream = frame(b"part");
        let end = {
            let mut f = frame(b"{\"error\":{\"code\":\"unavailable\"}}");
            f[0] = FLAG_END;
            f
        };
        stream.extend_from_slice(&end);
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(&stream).unwrap();
        assert_eq!(2, frames.len());
        assert!(!frames[0].end);
        assert!(frames[1].end);
    }

    #[test]
    fn gzip_flag_is_decompressed() {
        use flate2::write::GzEncoder;
        use std::io::Write;
        let raw = b"the quick brown fox";
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(raw).unwrap();
        let gz = encoder.finish().unwrap();
        let mut encoded = Vec::new();
        encoded.push(FLAG_GZIP);
        encoded.extend_from_slice(&(gz.len() as u32).to_be_bytes());
        encoded.extend_from_slice(&gz);
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(&encoded).unwrap();
        assert_eq!(raw, frames[0].data.as_slice());
    }

    #[test]
    fn an_oversized_frame_length_is_rejected() {
        let mut encoded = Vec::new();
        encoded.push(0);
        encoded.extend_from_slice(&((64u32 << 20) + 1).to_be_bytes());
        let mut decoder = FrameDecoder::new();
        assert!(decoder.push(&encoded).is_err(), "畸形长度必须当场止步");
    }
}
