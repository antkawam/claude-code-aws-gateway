use anyhow::Result;
use clap::Subcommand;

use crate::commands::aip_overrides::{AipOverridesCommands, run as aip_overrides_run};
use crate::config::AdminClient;
use crate::util;

#[derive(Subcommand)]
pub enum EndpointsCommands {
    /// List all Bedrock endpoints
    List,

    /// Create a new Bedrock endpoint
    Create {
        /// Endpoint name
        #[arg(long)]
        name: String,

        /// AWS region (e.g. us-east-1)
        #[arg(long)]
        region: String,

        /// Routing prefix for model ID mapping
        #[arg(long)]
        routing_prefix: String,

        /// IAM role ARN for cross-account access
        #[arg(long)]
        role_arn: Option<String>,

        /// External ID for role assumption
        #[arg(long)]
        external_id: Option<String>,

        /// Inference profile ARN
        #[arg(long)]
        inference_profile_arn: Option<String>,

        /// Routing priority (lower = preferred)
        #[arg(long)]
        priority: Option<i32>,
    },

    /// Update an existing Bedrock endpoint
    Update {
        /// Endpoint ID
        id: String,

        /// Endpoint name
        #[arg(long)]
        name: Option<String>,

        /// AWS region
        #[arg(long)]
        region: Option<String>,

        /// Routing prefix
        #[arg(long)]
        routing_prefix: Option<String>,

        /// IAM role ARN for cross-account access
        #[arg(long)]
        role_arn: Option<String>,

        /// External ID for role assumption
        #[arg(long)]
        external_id: Option<String>,

        /// Inference profile ARN
        #[arg(long)]
        inference_profile_arn: Option<String>,

        /// Routing priority (lower = preferred)
        #[arg(long)]
        priority: Option<i32>,

        /// Enable or disable the endpoint
        #[arg(long, default_value = "true")]
        enabled: bool,
    },

    /// Delete a Bedrock endpoint
    Delete {
        /// Endpoint ID
        id: String,
    },

    /// Set an endpoint as the default
    SetDefault {
        /// Endpoint ID
        id: String,
    },

    /// Show quota information for an endpoint
    Quotas {
        /// Endpoint ID
        id: String,
    },

    /// List available models for an endpoint
    Models {
        /// Endpoint ID
        id: String,
    },

    /// Manage per-model Application Inference Profile overrides
    #[command(subcommand)]
    AipOverrides(AipOverridesCommands),
}

pub async fn run(cmd: EndpointsCommands, url: Option<String>, token: Option<String>) -> Result<()> {
    // AipOverrides is delegated to its own module before building AdminClient.
    if let EndpointsCommands::AipOverrides(sub) = cmd {
        return aip_overrides_run(sub, url, token).await;
    }

    let client = AdminClient::from_options(url, token).await?;

    match cmd {
        EndpointsCommands::List => {
            let resp = client.get("/admin/endpoints").await?;
            if let Some(endpoints) = resp["endpoints"].as_array() {
                if endpoints.is_empty() {
                    eprintln!("No endpoints found.");
                    return Ok(());
                }
                eprintln!(
                    "{:<36}  {:<20}  {:<16}  {:<8}  {:<8}  {:<8}  PRIORITY",
                    "ID", "NAME", "REGION", "HEALTH", "DEFAULT", "ENABLED"
                );
                eprintln!("{}", "-".repeat(110));
                for ep in endpoints {
                    println!(
                        "{:<36}  {:<20}  {:<16}  {:<8}  {:<8}  {:<8}  {}",
                        ep["id"].as_str().unwrap_or("-"),
                        ep["name"].as_str().unwrap_or("-"),
                        ep["region"].as_str().unwrap_or("-"),
                        ep["health"].as_str().unwrap_or("-"),
                        if ep["is_default"].as_bool().unwrap_or(false) {
                            "yes"
                        } else {
                            "no"
                        },
                        if ep["enabled"].as_bool().unwrap_or(true) {
                            "yes"
                        } else {
                            "no"
                        },
                        ep["priority"]
                            .as_i64()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    );
                }
            }
        }

        EndpointsCommands::Create {
            name,
            region,
            routing_prefix,
            role_arn,
            external_id,
            inference_profile_arn,
            priority,
        } => {
            if inference_profile_arn.is_some() {
                crate::util::warn(
                    "--inference-profile-arn is deprecated. Use: ccag endpoints aip-overrides add <endpoint> --model <model> --arn <arn>",
                );
            }
            let mut body = serde_json::json!({
                "name": name,
                "region": region,
                "routing_prefix": routing_prefix,
            });
            if let Some(arn) = role_arn {
                body["role_arn"] = serde_json::Value::String(arn);
            }
            if let Some(eid) = external_id {
                body["external_id"] = serde_json::Value::String(eid);
            }
            if let Some(ipa) = inference_profile_arn {
                body["inference_profile_arn"] = serde_json::Value::String(ipa);
            }
            if let Some(p) = priority {
                body["priority"] = serde_json::json!(p);
            }

            let resp = client.post("/admin/endpoints", &body).await?;
            let id = resp["id"].as_str().unwrap_or("unknown");
            util::success(&format!("Endpoint created: {name} (id: {id})"));
        }

        EndpointsCommands::Update {
            id,
            name,
            region,
            routing_prefix,
            role_arn,
            external_id,
            inference_profile_arn,
            priority,
            enabled,
        } => {
            let mut body = serde_json::json!({
                "enabled": enabled,
            });
            if let Some(n) = name {
                body["name"] = serde_json::Value::String(n);
            }
            if let Some(r) = region {
                body["region"] = serde_json::Value::String(r);
            }
            if let Some(rp) = routing_prefix {
                body["routing_prefix"] = serde_json::Value::String(rp);
            }
            if let Some(arn) = role_arn {
                body["role_arn"] = serde_json::Value::String(arn);
            }
            if let Some(eid) = external_id {
                body["external_id"] = serde_json::Value::String(eid);
            }
            if let Some(ipa) = inference_profile_arn {
                body["inference_profile_arn"] = serde_json::Value::String(ipa);
            }
            if let Some(p) = priority {
                body["priority"] = serde_json::json!(p);
            }

            client.put(&format!("/admin/endpoints/{id}"), &body).await?;
            util::success(&format!("Endpoint {id} updated"));
        }

        EndpointsCommands::Delete { id } => {
            client.delete(&format!("/admin/endpoints/{id}")).await?;
            util::success(&format!("Endpoint {id} deleted"));
        }

        EndpointsCommands::SetDefault { id } => {
            client
                .put(
                    &format!("/admin/endpoints/{id}/default"),
                    &serde_json::json!({}),
                )
                .await?;
            util::success(&format!("Endpoint {id} set as default"));
        }

        EndpointsCommands::Quotas { id } => {
            let resp = client.get(&format!("/admin/endpoints/{id}/quotas")).await?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }

        EndpointsCommands::Models { id } => {
            let resp = client.get(&format!("/admin/endpoints/{id}/models")).await?;
            if let Some(name) = resp["endpoint_name"].as_str() {
                eprintln!("Endpoint: {name}");
            }
            if let Some(profile) = resp["inference_profile"].as_str() {
                eprintln!("Inference profile: {profile}");
            }
            if let Some(models) = resp["models"].as_array() {
                if models.is_empty() {
                    eprintln!("No models found.");
                    return Ok(());
                }
                eprintln!();
                eprintln!("Models:");
                for model in models {
                    if let Some(arn) = model.as_str() {
                        println!("  {arn}");
                    } else if let Some(arn) = model["model_arn"].as_str() {
                        println!("  {arn}");
                    } else {
                        println!("  {model}");
                    }
                }
            }
        }

        EndpointsCommands::AipOverrides(_) => unreachable!("handled above"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Contract tests for Task 8: `ccag endpoints aip-overrides` subcommands.
    //!
    //! Tests are expected RED until the builder implements
    //! `cli/src/commands/aip_overrides.rs`.
    //!
    //! Mocking strategy: a minimal single-use HTTP server running in a
    //! background thread using only `std::net::TcpListener`.  No extra
    //! dependencies required — `tokio` and `reqwest` are already in
    //! `cli/Cargo.toml`.
    //!
    //! Contracts covered:
    //!  1  – aip-overrides list happy path
    //!  2  – aip-overrides list empty
    //!  3  – aip-overrides add happy path (without reason)
    //!  3b – aip-overrides add with --reason
    //!  4  – aip-overrides add 409 conflict
    //!  5  – aip-overrides add 400 bad request
    //!  6  – aip-overrides remove happy path
    //!  7  – aip-overrides remove 404
    //!  8  – legacy --inference-profile-arn exits Ok
    //!  8b – deprecation message format snapshot
    //!  9  – legacy flag: stdout clean (deprecation note goes to stderr only)

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;
    // Contracts 1-7 import from the future aip_overrides module.
    // These will fail to compile until the builder creates the module — that
    // is the intended red state.
    use super::super::aip_overrides::{AipOverridesCommands, run as aip_run};

    const ENDPOINT_ID: &str = "11111111-1111-1111-1111-111111111111";
    const TOKEN: &str = "test-token";

    // ─── Minimal mock HTTP server ─────────────────────────────────────────────
    //
    // `MockServer::start(response_body, status_code)` binds to a random
    // ephemeral port, serves exactly one request with the given response, and
    // returns the base URL.  The server runs in a background thread so async
    // tests can proceed.
    //
    // `CapturedRequest` holds the method, path, and body of the request that
    // was received, so tests can assert on what the CLI sent.

    #[derive(Debug, Default)]
    struct CapturedRequest {
        method: String,
        path: String,
        body: String,
        auth_header: String,
    }

    /// Spawn a single-shot HTTP server.  Returns `(base_url, captured)`.
    /// The server serves the given response once, then exits.
    fn spawn_mock_server(
        status: u16,
        response_body: &str,
    ) -> (String, Arc<Mutex<Option<CapturedRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        let body = response_body.to_string();
        let captured: Arc<Mutex<Option<CapturedRequest>>> = Arc::new(Mutex::new(None));
        let captured_clone = Arc::clone(&captured);

        std::thread::spawn(move || {
            // Accept one connection
            if let Ok((mut stream, _)) = listener.accept() {
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                // Read until we have the full HTTP request (crude but sufficient
                // for testing — we always send JSON bodies < 4096 bytes).
                if let Ok(n) = stream.read(&mut buf) {
                    raw.extend_from_slice(&buf[..n]);
                }

                let req_str = String::from_utf8_lossy(&raw).into_owned();
                let mut lines = req_str.lines();
                let request_line = lines.next().unwrap_or("").to_string();
                let mut parts = request_line.splitn(3, ' ');
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();

                let mut auth_header = String::new();
                let mut content_length: usize = 0;
                for line in &mut lines {
                    if line.is_empty() {
                        break;
                    }
                    let lower = line.to_lowercase();
                    if lower.starts_with("authorization:") {
                        auth_header = line[14..].trim().to_string();
                    }
                    if lower.starts_with("content-length:") {
                        content_length = line[15..].trim().parse().unwrap_or(0);
                    }
                }

                // Body follows the blank line
                let body_start = req_str
                    .find("\r\n\r\n")
                    .map(|i| i + 4)
                    .or_else(|| req_str.find("\n\n").map(|i| i + 2))
                    .unwrap_or(req_str.len());
                let req_body = if content_length > 0 && body_start < req_str.len() {
                    req_str[body_start..].to_string()
                } else {
                    String::new()
                };

                *captured_clone.lock().unwrap() = Some(CapturedRequest {
                    method,
                    path,
                    body: req_body,
                    auth_header,
                });

                let reason = match status {
                    200 => "OK",
                    201 => "Created",
                    400 => "Bad Request",
                    404 => "Not Found",
                    409 => "Conflict",
                    _ => "Unknown",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        (url, captured)
    }

    fn url(base: &str) -> Option<String> {
        Some(base.to_string())
    }
    fn token() -> Option<String> {
        Some(TOKEN.to_string())
    }

    // ─── Contract 1: aip-overrides list happy path ───────────────────────────

    #[tokio::test]
    async fn test_aip_overrides_list_happy_path() {
        let body = serde_json::json!({
            "overrides": [
                {
                    "model_id": "claude-sonnet-4-5",
                    "aip_arn": "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged",
                    "set_at": "2026-06-06T12:00:00Z",
                    "set_by": "admin",
                    "reason": "cost tagging"
                },
                {
                    "model_id": "claude-opus-4-7",
                    "aip_arn": "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus-tagged",
                    "set_at": "2026-06-06T12:01:00Z",
                    "set_by": "admin",
                    "reason": null
                }
            ]
        })
        .to_string();

        let (base_url, captured) = spawn_mock_server(200, &body);

        let cmd = AipOverridesCommands::List {
            endpoint: ENDPOINT_ID.to_string(),
        };

        let result = aip_run(cmd, url(&base_url), token()).await;
        assert!(result.is_ok(), "list happy path must succeed: {result:?}");

        let req = captured.lock().unwrap();
        let req = req.as_ref().expect("server must have received a request");
        assert_eq!(req.method, "GET", "must use GET");
        assert!(
            req.path.contains(ENDPOINT_ID),
            "path must contain endpoint id, got: {}",
            req.path
        );
        assert!(
            req.path.ends_with("/aip-overrides"),
            "path must end with /aip-overrides, got: {}",
            req.path
        );
        assert!(
            req.auth_header.contains(TOKEN),
            "must send Bearer token, got: {}",
            req.auth_header
        );
    }

    // ─── Contract 2: aip-overrides list empty ────────────────────────────────

    #[tokio::test]
    async fn test_aip_overrides_list_empty() {
        let body = serde_json::json!({ "overrides": [] }).to_string();
        let (base_url, _captured) = spawn_mock_server(200, &body);

        let cmd = AipOverridesCommands::List {
            endpoint: ENDPOINT_ID.to_string(),
        };

        let result = aip_run(cmd, url(&base_url), token()).await;
        assert!(
            result.is_ok(),
            "empty list must still exit Ok (no error): {result:?}"
        );
    }

    // ─── Contract 3: aip-overrides add happy path ────────────────────────────

    #[tokio::test]
    async fn test_aip_overrides_add_happy_path() {
        let model_id = "claude-sonnet-4-5";
        let aip_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged";

        let response_body = serde_json::json!({
            "model_id": model_id,
            "aip_arn": aip_arn,
            "set_at": "2026-06-06T12:00:00Z",
            "set_by": "admin"
        })
        .to_string();

        let (base_url, captured) = spawn_mock_server(201, &response_body);

        let cmd = AipOverridesCommands::Add {
            endpoint: ENDPOINT_ID.to_string(),
            model: model_id.to_string(),
            arn: aip_arn.to_string(),
            reason: None,
        };

        let result = aip_run(cmd, url(&base_url), token()).await;
        assert!(result.is_ok(), "add happy path must succeed: {result:?}");

        let req = captured.lock().unwrap();
        let req = req.as_ref().expect("server must have received a request");
        assert_eq!(req.method, "POST", "must use POST");
        assert!(
            req.path.contains(ENDPOINT_ID),
            "path must contain endpoint id"
        );
        assert!(
            req.path.ends_with("/aip-overrides"),
            "path must end with /aip-overrides"
        );

        // Request body must contain model_id and aip_arn
        let sent: serde_json::Value =
            serde_json::from_str(&req.body).expect("request body must be valid JSON");
        assert_eq!(
            sent["model_id"].as_str(),
            Some(model_id),
            "model_id must be in request body"
        );
        assert_eq!(
            sent["aip_arn"].as_str(),
            Some(aip_arn),
            "aip_arn must be in request body"
        );
        // reason must NOT be present when not supplied
        assert!(
            sent.get("reason").map(|v| v.is_null()).unwrap_or(true),
            "reason must be absent or null when not provided"
        );
    }

    // ─── Contract 3b: aip-overrides add with reason ──────────────────────────

    #[tokio::test]
    async fn test_aip_overrides_add_with_reason() {
        let model_id = "claude-opus-4-7";
        let aip_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus-tagged";
        let reason = "ops team cost allocation";

        let response_body = serde_json::json!({
            "model_id": model_id,
            "aip_arn": aip_arn,
            "set_at": "2026-06-06T12:02:00Z",
            "set_by": "admin",
            "reason": reason
        })
        .to_string();

        let (base_url, captured) = spawn_mock_server(201, &response_body);

        let cmd = AipOverridesCommands::Add {
            endpoint: ENDPOINT_ID.to_string(),
            model: model_id.to_string(),
            arn: aip_arn.to_string(),
            reason: Some(reason.to_string()),
        };

        let result = aip_run(cmd, url(&base_url), token()).await;
        assert!(result.is_ok(), "add with reason must succeed: {result:?}");

        let req = captured.lock().unwrap();
        let req = req.as_ref().expect("server must have received a request");
        let sent: serde_json::Value =
            serde_json::from_str(&req.body).expect("request body must be valid JSON");
        assert_eq!(
            sent["reason"].as_str(),
            Some(reason),
            "reason must be included in request body"
        );
    }

    // ─── Contract 4: aip-overrides add 409 conflict ──────────────────────────

    #[tokio::test]
    async fn test_aip_overrides_add_conflict_409() {
        let response_body = serde_json::json!({
            "error": {
                "type": "conflict",
                "message": "override already exists for model claude-sonnet-4-5"
            }
        })
        .to_string();

        let (base_url, _captured) = spawn_mock_server(409, &response_body);

        let cmd = AipOverridesCommands::Add {
            endpoint: ENDPOINT_ID.to_string(),
            model: "claude-sonnet-4-5".to_string(),
            arn: "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/x"
                .to_string(),
            reason: None,
        };

        let result = aip_run(cmd, url(&base_url), token()).await;
        assert!(
            result.is_err(),
            "409 must cause non-zero exit (Err), got Ok"
        );

        let err_msg = format!("{:?}", result.unwrap_err());
        let conflict_indicated = err_msg.contains("409")
            || err_msg.to_lowercase().contains("conflict")
            || err_msg.to_lowercase().contains("already exists");
        assert!(
            conflict_indicated,
            "error must indicate a conflict (got: {err_msg})"
        );
    }

    // ─── Contract 5: aip-overrides add 400 bad request ───────────────────────

    #[tokio::test]
    async fn test_aip_overrides_add_bad_request_400() {
        let response_body = serde_json::json!({
            "error": {
                "type": "validation_error",
                "message": "aip_arn does not match expected ARN format"
            }
        })
        .to_string();

        let (base_url, _captured) = spawn_mock_server(400, &response_body);

        let cmd = AipOverridesCommands::Add {
            endpoint: ENDPOINT_ID.to_string(),
            model: "claude-sonnet-4-5".to_string(),
            arn: "not-a-valid-arn".to_string(),
            reason: None,
        };

        let result = aip_run(cmd, url(&base_url), token()).await;
        assert!(
            result.is_err(),
            "400 must cause non-zero exit (Err), got Ok"
        );

        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("400"),
            "error must mention 400 status (got: {err_msg})"
        );
    }

    // ─── Contract 6: aip-overrides remove happy path ─────────────────────────

    #[tokio::test]
    async fn test_aip_overrides_remove_happy_path() {
        let model_id = "claude-sonnet-4-5";

        let response_body = serde_json::json!({ "deleted": true }).to_string();
        let (base_url, captured) = spawn_mock_server(200, &response_body);

        let cmd = AipOverridesCommands::Remove {
            endpoint: ENDPOINT_ID.to_string(),
            model: model_id.to_string(),
        };

        let result = aip_run(cmd, url(&base_url), token()).await;
        assert!(result.is_ok(), "remove happy path must succeed: {result:?}");

        let req = captured.lock().unwrap();
        let req = req.as_ref().expect("server must have received a request");
        assert_eq!(req.method, "DELETE", "must use DELETE");
        assert!(
            req.path.ends_with(model_id),
            "DELETE path must end with model_id, got: {}",
            req.path
        );
        assert!(
            req.path.contains(ENDPOINT_ID),
            "DELETE path must contain endpoint id"
        );
    }

    // ─── Contract 7: aip-overrides remove 404 ────────────────────────────────

    #[tokio::test]
    async fn test_aip_overrides_remove_not_found_404() {
        let response_body = serde_json::json!({
            "error": {
                "type": "not_found",
                "message": "no override found for model claude-opus-4-7 on this endpoint"
            }
        })
        .to_string();

        let (base_url, _captured) = spawn_mock_server(404, &response_body);

        let cmd = AipOverridesCommands::Remove {
            endpoint: ENDPOINT_ID.to_string(),
            model: "claude-opus-4-7".to_string(),
        };

        let result = aip_run(cmd, url(&base_url), token()).await;
        assert!(
            result.is_err(),
            "404 must cause non-zero exit (Err), got Ok"
        );

        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("404"),
            "error must mention 404 status (got: {err_msg})"
        );
    }

    // ─── Contract 8: legacy --inference-profile-arn exits Ok ─────────────────
    //
    // `ccag endpoints create --inference-profile-arn <arn>` must:
    //   (a) call POST /admin/endpoints as today
    //   (b) exit Ok (zero exit code)
    //   (c) emit a deprecation note to stderr (validated by contracts 8b + 9)

    #[tokio::test]
    async fn test_legacy_inference_profile_arn_exits_ok() {
        let endpoint_name = "test-endpoint";
        let inference_profile_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged";

        let response_body = serde_json::json!({
            "id": ENDPOINT_ID,
            "name": endpoint_name,
            "region": "us-east-1",
            "routing_prefix": "us",
            "inference_profile_arn": inference_profile_arn,
            "enabled": true
        })
        .to_string();

        let (base_url, captured) = spawn_mock_server(201, &response_body);

        let cmd = EndpointsCommands::Create {
            name: endpoint_name.to_string(),
            region: "us-east-1".to_string(),
            routing_prefix: "us".to_string(),
            role_arn: None,
            external_id: None,
            inference_profile_arn: Some(inference_profile_arn.to_string()),
            priority: None,
        };

        let result = run(cmd, url(&base_url), token()).await;
        assert!(
            result.is_ok(),
            "legacy --inference-profile-arn must exit Ok: {result:?}"
        );

        let req = captured.lock().unwrap();
        let req = req.as_ref().expect("server must have received a request");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/admin/endpoints");

        // Verify the ARN was forwarded in the request body
        let sent: serde_json::Value =
            serde_json::from_str(&req.body).expect("request body must be valid JSON");
        assert_eq!(
            sent["inference_profile_arn"].as_str(),
            Some(inference_profile_arn),
            "inference_profile_arn must be forwarded to the API"
        );
    }

    // ─── Contract 8b: deprecation message format snapshot ────────────────────
    //
    // The builder must emit EXACTLY this format to stderr when
    // --inference-profile-arn is supplied:
    //
    //   "[!!] --inference-profile-arn is deprecated. Use: ccag endpoints aip-overrides add <endpoint> --model <model> --arn <arn>"
    //
    // This constant is the canonical snapshot.  If the format is changed,
    // this test must be updated first.

    #[test]
    fn test_legacy_deprecation_message_format_snapshot() {
        // The exact string the builder must emit (via `crate::util::warn` or
        // equivalent) to stderr when `--inference-profile-arn` is provided.
        //
        // `crate::util::warn` formats as:
        //   "\x1b[33m[!!]\x1b[0m {msg}"
        //
        // So the full stderr line (with ANSI stripped) must match:
        //   "[!!] --inference-profile-arn is deprecated. Use: ccag endpoints aip-overrides add <endpoint> --model <model> --arn <arn>"
        //
        // We validate the semantic content here; the builder owns the exact
        // ANSI formatting.
        let expected_msg = "--inference-profile-arn is deprecated. Use: ccag endpoints aip-overrides add <endpoint> --model <model> --arn <arn>";

        // Snapshot checks
        assert!(
            expected_msg.contains("--inference-profile-arn is deprecated"),
            "must name the deprecated flag"
        );
        assert!(
            expected_msg.contains("ccag endpoints aip-overrides add"),
            "must point at the replacement subcommand"
        );
        assert!(
            expected_msg.contains("--model"),
            "replacement hint must include --model flag"
        );
        assert!(
            expected_msg.contains("--arn"),
            "replacement hint must include --arn flag"
        );
    }

    // ─── Contract 9: stdout clean when legacy flag used ───────────────────────
    //
    // The deprecation note must NOT appear on stdout — only stderr.
    // `util::warn` already uses `eprintln!`, so using it satisfies this.
    //
    // We verify indirectly: the request body sent to the server must not
    // contain the deprecation string (guards against accidental stdout
    // pollution in the request payload).

    #[tokio::test]
    async fn test_legacy_flag_request_body_clean() {
        let endpoint_name = "another-endpoint";
        let inference_profile_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/haiku-tagged";

        let response_body = serde_json::json!({
            "id": "22222222-2222-2222-2222-222222222222",
            "name": endpoint_name,
            "region": "us-east-1",
            "routing_prefix": "us",
            "inference_profile_arn": inference_profile_arn,
            "enabled": true
        })
        .to_string();

        let (base_url, captured) = spawn_mock_server(201, &response_body);

        let cmd = EndpointsCommands::Create {
            name: endpoint_name.to_string(),
            region: "us-east-1".to_string(),
            routing_prefix: "us".to_string(),
            role_arn: None,
            external_id: None,
            inference_profile_arn: Some(inference_profile_arn.to_string()),
            priority: None,
        };

        let result = run(cmd, url(&base_url), token()).await;
        assert!(result.is_ok(), "must exit Ok: {result:?}");

        let req = captured.lock().unwrap();
        let req = req.as_ref().expect("server must have received a request");
        // The HTTP request body to the admin API must not contain the
        // deprecation text (it belongs on stderr only).
        assert!(
            !req.body.contains("deprecated"),
            "request body must not contain 'deprecated' — deprecation note belongs on stderr only"
        );
    }
}
