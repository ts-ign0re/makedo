//! Общая HTTP/SSE-машинерия обоих провайдеров: отправка запроса с ретраями,
//! декодирование SSE, прогон через диалект-специфичный маппер.
//! Отмена = drop стрима → drop HTTP-соединения (reqwest abort).

use std::collections::VecDeque;
use std::pin::Pin;

use futures_util::{Future, Stream, StreamExt};

use crate::sse::{SseDecoder, SseEvent};
use crate::{EventStream, LlmEvent, ProviderError};

/// Диалект-специфичное отображение SSE → события. Стейт — внутри маппера.
pub(crate) trait EventMapper: Send + 'static {
    fn map(&mut self, ev: SseEvent) -> Result<Vec<LlmEvent>, ProviderError>;
    /// Стрим байтов закончился без явного финального события.
    fn finish(&mut self) -> Result<Vec<LlmEvent>, ProviderError>;
}

const MAX_RETRIES: u32 = 3;

/// Отправка с ретраями на 429/5xx (экспонента 500мс·2^n; Retry-After не парсим —
/// ponytail: добавить при реальных упорах в лимиты).
pub(crate) async fn send_with_retries(
    builder: impl Fn() -> reqwest::RequestBuilder + Send,
) -> Result<reqwest::Response, ProviderError> {
    let mut attempt = 0u32;
    loop {
        let resp = builder().send().await;
        let retryable = match &resp {
            Ok(r) => {
                let s = r.status();
                s.as_u16() == 429 || s.is_server_error()
            }
            Err(e) => e.is_connect() || e.is_timeout(),
        };
        if retryable && attempt < MAX_RETRIES {
            attempt += 1;
            tokio::time::sleep(std::time::Duration::from_millis(500 * (1 << attempt))).await;
            continue;
        }
        let resp = resp?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api {
                status: status.as_u16(),
                body,
            });
        }
        return Ok(resp);
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;
type RespFuture = Pin<Box<dyn Future<Output = Result<reqwest::Response, ProviderError>> + Send>>;

struct Ctx<M> {
    fut: Option<RespFuture>,
    body: Option<ByteStream>,
    dec: SseDecoder,
    mapper: M,
    queue: VecDeque<Result<LlmEvent, ProviderError>>,
    finished: bool,
}

/// Собирает `EventStream` из будущего HTTP-ответа и маппера.
pub(crate) fn event_stream<M: EventMapper>(fut: RespFuture, mapper: M) -> EventStream {
    let ctx = Ctx {
        fut: Some(fut),
        body: None,
        dec: SseDecoder::default(),
        mapper,
        queue: VecDeque::new(),
        finished: false,
    };
    Box::pin(futures_util::stream::unfold(ctx, |mut c| async move {
        loop {
            if let Some(item) = c.queue.pop_front() {
                return Some((item, c));
            }
            if c.finished {
                return None;
            }
            if let Some(fut) = c.fut.take() {
                match fut.await {
                    Ok(resp) => c.body = Some(Box::pin(resp.bytes_stream())),
                    Err(e) => {
                        c.finished = true;
                        return Some((Err(e), c));
                    }
                }
            }
            let body = c.body.as_mut()?;
            match body.next().await {
                Some(Ok(chunk)) => {
                    for ev in c.dec.feed(&chunk) {
                        match c.mapper.map(ev) {
                            Ok(evs) => c.queue.extend(evs.into_iter().map(Ok)),
                            Err(e) => {
                                c.queue.push_back(Err(e));
                                c.finished = true;
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    c.finished = true;
                    return Some((Err(e.into()), c));
                }
                None => {
                    c.finished = true;
                    match c.mapper.finish() {
                        Ok(evs) => c.queue.extend(evs.into_iter().map(Ok)),
                        Err(e) => c.queue.push_back(Err(e)),
                    }
                }
            }
        }
    }))
}
