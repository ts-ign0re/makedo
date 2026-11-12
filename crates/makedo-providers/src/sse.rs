//! Самописный SSE-декодер (~80 строк): формат тривиален, reqwest-eventsource
//! отвергнут как лишний dep (PLAN §3.8). Инкрементальный: скармливаются байтовые
//! чанки, события отдаются по мере завершения (`\n\n`). UTF-8-safe: незавершённый
//! хвост живёт в буфере байтов, не строк.

#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseDecoder {
    /// Скормить чанк; вернуть все полностью полученные события.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        // граница события — пустая строка (\n\n; \r\n нормализуем при разборе строк)
        while let Some(pos) = find_boundary(&self.buf) {
            let raw: Vec<u8> = self.buf.drain(..pos.end).collect();
            let block = String::from_utf8_lossy(&raw[..pos.start]);
            if let Some(ev) = parse_block(&block) {
                events.push(ev);
            }
        }
        events
    }
}

struct Boundary {
    start: usize,
    end: usize,
}

fn find_boundary(buf: &[u8]) -> Option<Boundary> {
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == b'\n' {
            // \n\n или \n\r\n
            if buf.get(i + 1) == Some(&b'\n') {
                return Some(Boundary {
                    start: i,
                    end: i + 2,
                });
            }
            if buf.get(i + 1) == Some(&b'\r') && buf.get(i + 2) == Some(&b'\n') {
                return Some(Boundary {
                    start: i,
                    end: i + 3,
                });
            }
        }
        i += 1;
    }
    None
}

fn parse_block(block: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in block.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim_start().to_owned());
        } else if let Some(v) = line.strip_prefix("data:") {
            data_lines.push(v.strip_prefix(' ').unwrap_or(v));
        }
        // комментарии (`:...`) и прочие поля (id, retry) игнорируем
    }
    if event.is_none() && data_lines.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data_lines.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_and_split_chunks() {
        let mut d = SseDecoder::default();
        assert!(d.feed(b"event: message_start\ndata: {\"a\"").is_empty());
        let evs = d.feed(b":1}\n\ndata: [DONE]\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event.as_deref(), Some("message_start"));
        assert_eq!(evs[0].data, "{\"a\":1}");
        assert_eq!(evs[1].data, "[DONE]");
    }

    #[test]
    fn crlf_and_comments() {
        let mut d = SseDecoder::default();
        let evs = d.feed(b": ping\r\n\r\nevent: e\r\ndata: x\r\ndata: y\r\n\r\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "x\ny");
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut d = SseDecoder::default();
        let msg = "data: привет\n\n".as_bytes();
        // рвём посреди многобайтового символа
        assert!(d.feed(&msg[..8]).is_empty());
        let evs = d.feed(&msg[8..]);
        assert_eq!(evs[0].data, "привет");
    }
}
