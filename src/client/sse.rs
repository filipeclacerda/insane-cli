//! Incremental Server-Sent-Events parser for NIM's chat streaming responses.
//!
//! Consumes bytes as they arrive (never buffers the whole body), splits on
//! `\n`, and yields `StreamChunk`s decoded from `data: {...}` lines. Tolerates
//! an invalid JSON payload in the middle of the stream by logging a warning
//! to stderr and continuing. Terminates on `data: [DONE]`.

use futures_util::{Stream, StreamExt};
use serde::Deserialize;

use super::{ChatStream, StreamChunk, ToolCallDelta, Usage};
use crate::error::ApiError;

#[derive(Debug, Deserialize)]
struct RawChunk {
    choices: Vec<RawChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct RawChoice {
    #[serde(default)]
    delta: RawDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<RawToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct RawToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<RawFunctionDelta>,
}

#[derive(Debug, Deserialize, Default)]
struct RawFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Parses a raw byte stream (as delivered by `reqwest`'s `bytes_stream`)
/// into a stream of `StreamChunk`s, per the SSE contract described above.
pub fn parse_sse<S, E>(bytes_stream: S) -> ChatStream
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let line_stream = futures_util::stream::unfold(
        (bytes_stream, Vec::<u8>::new(), false),
        |(mut stream, mut buf, mut done)| async move {
            loop {
                if done {
                    return None;
                }
                // Try to peel a complete line off the front of `buf`.
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let mut line: Vec<u8> = buf.drain(..=pos).collect();
                    if line.last() == Some(&b'\n') {
                        line.pop();
                    }
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Some((Ok(line), (stream, buf, done)));
                }
                match stream.next().await {
                    Some(Ok(bytes)) => buf.extend_from_slice(&bytes),
                    Some(Err(e)) => {
                        done = true;
                        return Some((Err(e.to_string()), (stream, buf, done)));
                    }
                    None => {
                        done = true;
                        if buf.is_empty() {
                            return None;
                        }
                        let line = std::mem::take(&mut buf);
                        return Some((Ok(line), (stream, buf, done)));
                    }
                }
            }
        },
    );

    let chunk_stream = line_stream.filter_map(|line_result| async move {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => return Some(Err(ApiError::transient(format!("stream read error: {e}")))),
        };
        let line = String::from_utf8_lossy(&line);
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let data = match line.strip_prefix("data:") {
            Some(d) => d.trim(),
            None => return None, // ignore non-data SSE fields (event:, id:, comments)
        };
        if data == "[DONE]" {
            return Some(Ok(None)); // sentinel: end of stream
        }
        match serde_json::from_str::<RawChunk>(data) {
            Ok(raw) => {
                let usage = raw.usage;
                let choice = raw.choices.into_iter().next();
                let (delta, tool_calls, finish_reason) = match choice {
                    Some(c) => {
                        let tool_calls = c
                            .delta
                            .tool_calls
                            .unwrap_or_default()
                            .into_iter()
                            .map(|t| ToolCallDelta {
                                index: t.index,
                                id: t.id,
                                name: t.function.as_ref().and_then(|f| f.name.clone()),
                                arguments: t.function.and_then(|f| f.arguments).unwrap_or_default(),
                            })
                            .collect();
                        (
                            c.delta.content.unwrap_or_default(),
                            tool_calls,
                            c.finish_reason,
                        )
                    }
                    None => (String::new(), Vec::new(), None),
                };
                Some(Ok(Some(StreamChunk {
                    delta,
                    tool_calls,
                    finish_reason,
                    usage,
                })))
            }
            Err(e) => {
                tracing::warn!("skipping malformed SSE chunk: {e}");
                None
            }
        }
    });

    Box::pin(
        chunk_stream
            .take_while(|item| {
                let keep = !matches!(item, Ok(None));
                async move { keep }
            })
            .filter_map(|item| async move {
                match item {
                    Ok(Some(chunk)) => Some(Ok(chunk)),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                }
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn bytes_chunks(
        chunks: &[&str],
    ) -> impl Stream<Item = Result<bytes::Bytes, std::convert::Infallible>> {
        let owned: Vec<Result<bytes::Bytes, std::convert::Infallible>> = chunks
            .iter()
            .map(|s| Ok(bytes::Bytes::from(s.as_bytes().to_vec())))
            .collect();
        stream::iter(owned)
    }

    async fn collect_deltas(s: ChatStream) -> Vec<String> {
        futures_util::pin_mut!(s);
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item.unwrap().delta);
        }
        out
    }

    #[tokio::test]
    async fn parses_simple_stream() {
        let input = [
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n",
            "data: [DONE]\n",
        ];
        let deltas = collect_deltas(parse_sse(bytes_chunks(&input))).await;
        assert_eq!(deltas, vec!["Hel", "lo"]);
    }

    #[tokio::test]
    async fn handles_split_across_reads() {
        // A single logical line arrives split across multiple byte chunks.
        let input = [
            "data: {\"choi",
            "ces\":[{\"delta\":{\"content\":\"ab\"}}]}\n",
            "data: [DONE]\n",
        ];
        let deltas = collect_deltas(parse_sse(bytes_chunks(&input))).await;
        assert_eq!(deltas, vec!["ab"]);
    }

    #[tokio::test]
    async fn tolerates_invalid_json_chunk_and_continues() {
        let input = [
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n",
            "data: {not valid json\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n",
            "data: [DONE]\n",
        ];
        let deltas = collect_deltas(parse_sse(bytes_chunks(&input))).await;
        assert_eq!(deltas, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn ignores_non_data_lines_and_blank_lines() {
        let input = [
            "event: message\n",
            "\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
            "\n",
            "data: [DONE]\n",
        ];
        let deltas = collect_deltas(parse_sse(bytes_chunks(&input))).await;
        assert_eq!(deltas, vec!["x"]);
    }

    #[tokio::test]
    async fn stops_at_done_even_with_trailing_data() {
        let input = [
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
            "data: [DONE]\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"should not appear\"}}]}\n",
        ];
        let deltas = collect_deltas(parse_sse(bytes_chunks(&input))).await;
        assert_eq!(deltas, vec!["x"]);
    }

    #[tokio::test]
    async fn handles_abrupt_cutoff_without_trailing_newline() {
        let input = ["data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}"];
        let deltas = collect_deltas(parse_sse(bytes_chunks(&input))).await;
        assert_eq!(deltas, vec!["x"]);
    }

    async fn collect_chunks(s: ChatStream) -> Vec<StreamChunk> {
        futures_util::pin_mut!(s);
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn accumulates_tool_call_arguments_fragmented_across_many_deltas() {
        // A single tool call's `arguments` string arrives split across 3+
        // deltas at the same `index`; the parser must hand each fragment
        // through untouched -- concatenation is the consumer's job
        // (agent.rs), per SPEC-AGENT §1/§6.
        let input = [
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"src/\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"main.rs\\\"}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        ];
        let chunks = collect_chunks(parse_sse(bytes_chunks(&input))).await;

        let mut id = None;
        let mut name = None;
        let mut args = String::new();
        let mut saw_finish = false;
        for chunk in &chunks {
            for tc in &chunk.tool_calls {
                assert_eq!(tc.index, 0);
                if let Some(i) = &tc.id {
                    id = Some(i.clone());
                }
                if let Some(n) = &tc.name {
                    name = Some(n.clone());
                }
                args.push_str(&tc.arguments);
            }
            if let Some(fr) = &chunk.finish_reason {
                assert_eq!(fr, "tool_calls");
                saw_finish = true;
            }
        }
        assert_eq!(id.as_deref(), Some("call_1"));
        assert_eq!(name.as_deref(), Some("read_file"));
        assert_eq!(args, "{\"path\":\"src/main.rs\"}");
        assert!(saw_finish);
    }

    #[tokio::test]
    async fn accumulates_multiple_tool_calls_by_index() {
        let input = [
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"list_files\",\"arguments\":\"{}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a\\\"}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        ];
        let chunks = collect_chunks(parse_sse(bytes_chunks(&input))).await;

        let mut by_index: std::collections::HashMap<usize, (String, String)> =
            std::collections::HashMap::new();
        for chunk in &chunks {
            for tc in &chunk.tool_calls {
                let entry = by_index
                    .entry(tc.index)
                    .or_insert_with(|| (String::new(), String::new()));
                if let Some(n) = &tc.name {
                    entry.0 = n.clone();
                }
                entry.1.push_str(&tc.arguments);
            }
        }
        assert_eq!(by_index.len(), 2);
        assert_eq!(by_index[&0].0, "list_files");
        assert_eq!(by_index[&1].0, "read_file");
        assert_eq!(by_index[&1].1, "{\"path\":\"a\"}");
    }
}
