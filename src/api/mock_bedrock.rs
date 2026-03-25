/// Mock Bedrock response generator for load testing.
///
/// Produces realistic SSE streams with configurable latency to simulate
/// real Bedrock behavior under load. Feature-gated behind `mock-bedrock`.
///
/// Environment variables:
/// - `MOCK_TTFT_MS`: Time to first token in ms (default: 800)
/// - `MOCK_CHUNK_DELAY_MS`: Delay between chunks in ms (default: 50)
/// - `MOCK_CHUNKS`: Number of content_block_delta events (default: 30)
/// - `MOCK_JITTER_PCT`: Random ±% jitter on delays (default: 20)
use std::time::Duration;

use axum::body::Body;
use axum::http::{Response, StatusCode, header};
use rand::Rng;
use tokio::time::sleep;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn jittered_delay(base_ms: u64, jitter_pct: u64) -> Duration {
    if jitter_pct == 0 || base_ms == 0 {
        return Duration::from_millis(base_ms);
    }
    let mut rng = rand::rng();
    let jitter_range = (base_ms as f64 * jitter_pct as f64 / 100.0) as i64;
    let jitter = rng.random_range(-jitter_range..=jitter_range);
    let actual = (base_ms as i64 + jitter).max(1) as u64;
    Duration::from_millis(actual)
}

/// Generate a mock streaming SSE response with realistic latency.
pub fn mock_streaming_response(original_model: &str, request_id: &str) -> Response<Body> {
    let ttft_ms = env_u64("MOCK_TTFT_MS", 800);
    let chunk_delay_ms = env_u64("MOCK_CHUNK_DELAY_MS", 50);
    let num_chunks = env_u64("MOCK_CHUNKS", 30) as usize;
    let jitter_pct = env_u64("MOCK_JITTER_PCT", 20);

    let model = original_model.to_string();
    let req_id = request_id.to_string();

    let stream = async_stream::stream! {
        // Simulate time to first token
        sleep(jittered_delay(ttft_ms, jitter_pct)).await;

        // message_start
        let msg_start = format!(
            "event: message_start\ndata: {}\n\n",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": req_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": 150,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0
                    }
                }
            })
        );
        yield Ok::<_, std::convert::Infallible>(msg_start);

        // content_block_start
        let block_start = format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            })
        );
        yield Ok(block_start);

        // content_block_delta events with inter-token latency
        for i in 0..num_chunks {
            sleep(jittered_delay(chunk_delay_ms, jitter_pct)).await;

            let word = match i % 5 {
                0 => "The ",
                1 => "quick ",
                2 => "brown ",
                3 => "fox ",
                _ => "jumps. ",
            };
            let delta = format!(
                "event: content_block_delta\ndata: {}\n\n",
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": word}
                })
            );
            yield Ok(delta);
        }

        // content_block_stop
        let block_stop = format!(
            "event: content_block_stop\ndata: {}\n\n",
            serde_json::json!({"type": "content_block_stop", "index": 0})
        );
        yield Ok(block_stop);

        // message_delta (final usage)
        let output_tokens = (num_chunks * 3) as u32;
        let msg_delta = format!(
            "event: message_delta\ndata: {}\n\n",
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": output_tokens}
            })
        );
        yield Ok(msg_delta);

        // message_stop
        let msg_stop = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string();
        yield Ok(msg_stop);
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Generate a mock non-streaming JSON response with TTFT delay.
pub fn mock_non_streaming_response(original_model: &str, request_id: &str) -> Response<Body> {
    let ttft_ms = env_u64("MOCK_TTFT_MS", 800);
    let jitter_pct = env_u64("MOCK_JITTER_PCT", 20);
    let num_chunks = env_u64("MOCK_CHUNKS", 30) as u32;

    let model = original_model.to_string();
    let req_id = request_id.to_string();
    let output_tokens = num_chunks * 3;

    let body = async move {
        sleep(jittered_delay(ttft_ms, jitter_pct)).await;

        let response = serde_json::json!({
            "id": req_id,
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "This is a mock response for load testing. The quick brown fox jumps over the lazy dog."}],
            "model": model,
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 150,
                "output_tokens": output_tokens,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        });

        serde_json::to_vec(&response).unwrap()
    };

    // Use a stream that yields once after the delay
    let stream = async_stream::stream! {
        let bytes = body.await;
        yield Ok::<_, std::convert::Infallible>(bytes);
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Input/output token counts for spend tracking.
pub fn mock_token_counts(num_chunks: Option<u64>) -> (i32, i32) {
    let chunks = num_chunks.unwrap_or_else(|| env_u64("MOCK_CHUNKS", 30));
    (150, (chunks * 3) as i32)
}
