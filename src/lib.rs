pub mod bootstrap;
pub mod config;
pub mod error;
pub mod http;
pub mod oco;
pub mod translate;
pub mod turn;

pub use config::{normalize_base, num_env, Config};
pub use error::{ClientAbortError, OcoTimeoutError, UpstreamError, ValidationError, WrapError};
pub use oco::{
    is_rate_limited_upstream, is_retryable_upstream, is_timeout_abort,
    is_transient_connection_error, oco, prompt_with_retry, upstream_http_status, OcoClient,
};
pub use translate::{
    apply_stop_and_max_tokens, collect_file_parts, guess_mime, msg_text, parse_chat_options,
    parse_event_block, parse_tool_calls, part_text, render_history, resolve_model_id, rid,
    tool_instruction, ChatOptions,
};
pub use turn::{handle_chat_completions, list_models, prepare_turn, stream_chat_completions};
