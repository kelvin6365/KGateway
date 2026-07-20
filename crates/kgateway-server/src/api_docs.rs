//! Documentation artifacts rendered from [`crate::api_catalog`].
//!
//! Four surfaces, one source: an OpenAPI 3.1 spec (which also feeds Postman, Insomnia,
//! and client generators), per-endpoint Markdown, an `llms.txt` index, and an
//! `llms-full.txt` with everything inlined. Because they all render from the catalog —
//! and the catalog is pinned to the router by `api_catalog::drift_tests` — none of them
//! can drift from what the gateway actually serves.
//!
//! All four are served unauthenticated: they describe the admin surface but contain no
//! secrets, and an agent pointed at a gateway should be able to discover its API.

use crate::api_catalog::{Auth, Endpoint, ENDPOINTS};

/// Groups in presentation order. Derived from the catalog rather than hard-coded, so a
/// new auth tier appears without touching the renderers.
fn groups() -> Vec<&'static str> {
    let order = [
        Auth::DataPlane,
        Auth::Public,
        Auth::LogsView,
        Auth::ConfigWrite,
        Auth::LogsReveal,
    ];
    let mut out: Vec<&'static str> = Vec::new();
    for a in order {
        let g = a.group();
        if !out.contains(&g) {
            out.push(g);
        }
    }
    out
}

fn in_group(group: &str) -> impl Iterator<Item = &'static Endpoint> + '_ {
    ENDPOINTS.iter().filter(move |e| e.auth.group() == group)
}

// ---------- OpenAPI 3.1 ----------

/// Render the whole catalog as an OpenAPI 3.1 document.
pub fn openapi(base_url: &str) -> serde_json::Value {
    let mut paths = serde_json::Map::new();

    for (index, e) in ENDPOINTS.iter().enumerate() {
        // OpenAPI path templating already uses `{name}`, which is also axum's syntax.
        let entry = paths
            .entry(e.path.to_string())
            .or_insert_with(|| serde_json::json!({}));

        let params: Vec<serde_json::Value> = e
            .params
            .iter()
            .filter(|p| matches!(p.location, "path" | "query"))
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "in": p.location,
                    "required": p.required || p.location == "path",
                    "description": p.description,
                    "schema": { "type": json_type(p.ty) },
                })
            })
            .collect();

        let body_params: Vec<_> = e.params.iter().filter(|p| p.location == "body").collect();
        let request_body = if body_params.is_empty() {
            serde_json::Value::Null
        } else {
            let mut props = serde_json::Map::new();
            let mut required: Vec<&str> = Vec::new();
            for p in &body_params {
                props.insert(
                    p.name.to_string(),
                    serde_json::json!({ "type": json_type(p.ty), "description": p.description }),
                );
                if p.required {
                    required.push(p.name);
                }
            }
            serde_json::json!({
                "required": true,
                "content": { "application/json": { "schema": {
                    "type": "object",
                    "properties": props,
                    "required": required,
                }}}
            })
        };

        let mut op = serde_json::json!({
            "summary": e.summary,
            "description": e.description,
            "operationId": e.slug().replace('-', "_"),
            "tags": [e.auth.group()],
            "parameters": params,
            "responses": {
                "200": { "description": "Success" },
                "401": { "description": "Missing or unknown credential" },
            },
            // Non-standard but widely used by doc tooling, and the thing readers need most.
            "x-kgateway-auth": e.auth.label(),
            // serde_json serializes maps in sorted key order, which would bury
            // /v1/chat/completions under /docs/{file}. Carry the catalog's order so
            // readers meet the important endpoints first.
            "x-order": index,
            "x-codeSamples": [{ "lang": "curl", "source": e.example }],
        });
        if !request_body.is_null() {
            op["requestBody"] = request_body;
        }
        if e.auth != Auth::Public {
            op["security"] = serde_json::json!([{ "bearerAuth": [] }]);
        }
        entry[e.method.to_lowercase()] = op;
    }

    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "KGateway",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "One OpenAI-compatible API in front of every major LLM provider, with \
    failover, governance, semantic caching, and per-request tracing.",
        },
        "servers": [{ "url": base_url }],
        "tags": groups().iter().map(|g| serde_json::json!({ "name": g })).collect::<Vec<_>>(),
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "Data-plane routes take a virtual key when governance is on; \
    control-plane routes take an RBAC token.",
                }
            }
        },
        "paths": paths,
    })
}

fn json_type(ty: &str) -> &'static str {
    match ty {
        "integer" => "integer",
        "number" => "number",
        "boolean" => "boolean",
        "array" => "array",
        t if t.starts_with("string |") || t == "string" => "string",
        _ => "string",
    }
}

// ---------- Markdown ----------

/// One endpoint as a standalone Markdown page — the `.md` twin the llms.txt index links to.
pub fn endpoint_markdown(e: &Endpoint, base_url: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {} {}\n\n", e.method, e.path));
    s.push_str(&format!("> {}\n\n", e.summary));
    s.push_str(&format!("**Auth:** {}\n\n", e.auth.label()));
    s.push_str(&format!("{}\n\n", e.description));

    if !e.params.is_empty() {
        s.push_str("## Parameters\n\n");
        s.push_str("| Name | In | Type | Required | Description |\n");
        s.push_str("|---|---|---|---|---|\n");
        for p in e.params {
            s.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |\n",
                p.name,
                p.location,
                p.ty,
                if p.required { "yes" } else { "no" },
                p.description
            ));
        }
        s.push('\n');
    }

    s.push_str("## Example\n\n```bash\n");
    s.push_str(&e.example.replace("http://localhost:8080", base_url));
    s.push_str("\n```\n");

    if !e.response.is_empty() {
        s.push_str("\n## Response\n\n```json\n");
        s.push_str(e.response);
        s.push_str("\n```\n");
    }
    s
}

/// Look up an endpoint by its slug, for `GET /docs/{slug}.md`.
pub fn endpoint_by_slug(slug: &str) -> Option<&'static Endpoint> {
    ENDPOINTS.iter().find(|e| e.slug() == slug)
}

// ---------- llms.txt ----------

/// The llms.txt index: a title, a one-line description, then a link per endpoint,
/// grouped by section. Follows the convention agents are trained on — an index of
/// links to `.md` pages, with `llms-full.txt` carrying the inlined version.
pub fn llms_txt(base_url: &str) -> String {
    let mut s = String::from("# KGateway\n\n");
    s.push_str(
        "> An OpenAI-compatible AI gateway in front of every major LLM provider, with provider \
failover, virtual-key governance, semantic caching, and per-request tracing. Route to any \
provider with a `provider/model` string.\n\n",
    );
    s.push_str(
        "Endpoints are grouped by the credential they need. Data-plane routes are open until \
virtual keys are configured; control-plane routes need an RBAC token.\n\n",
    );

    for group in groups() {
        s.push_str(&format!("## {group}\n\n"));
        for e in in_group(group) {
            s.push_str(&format!(
                "- [{} {}]({}/docs/{}.md): {}\n",
                e.method,
                e.path,
                base_url,
                e.slug(),
                e.summary
            ));
        }
        s.push('\n');
    }

    s.push_str("## Optional\n\n");
    s.push_str(&format!(
        "- [OpenAPI 3.1 specification]({base_url}/openapi.json): The whole API as a standard spec.\n"
    ));
    s.push_str(&format!(
        "- [Full documentation]({base_url}/llms-full.txt): Every endpoint inlined in one file.\n"
    ));
    s
}

/// Everything inlined, for pasting into a model's context in a single fetch.
pub fn llms_full_txt(base_url: &str) -> String {
    let mut s = String::from("# KGateway — full API reference\n\n");
    s.push_str(
        "> Generated from the gateway's route table. Every endpoint below is served by this \
instance.\n\n",
    );
    for group in groups() {
        s.push_str(&format!("---\n\n# {group}\n\n"));
        for e in in_group(group) {
            s.push_str(&endpoint_markdown(e, base_url));
            s.push('\n');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "http://localhost:8080";

    #[test]
    fn openapi_is_valid_shaped_and_covers_every_endpoint() {
        let spec = openapi(BASE);
        assert_eq!(spec["openapi"], "3.1.0");
        assert_eq!(spec["servers"][0]["url"], BASE);

        let paths = spec["paths"].as_object().unwrap();
        for e in ENDPOINTS {
            let op = &paths[e.path][e.method.to_lowercase()];
            assert!(
                op.is_object(),
                "{} {} missing from the spec",
                e.method,
                e.path
            );
            assert_eq!(op["summary"], e.summary);
            assert_eq!(op["x-kgateway-auth"], e.auth.label());
        }
    }

    #[test]
    fn catalog_order_is_preserved_for_renderers() {
        // Map keys serialize sorted, so without this hint the reference would open on
        // /docs/{file} instead of the endpoint everyone actually wants.
        let spec = openapi(BASE);
        let chat = spec["paths"]["/v1/chat/completions"]["post"]["x-order"]
            .as_u64()
            .unwrap();
        let docs = spec["paths"]["/docs/{file}"]["get"]["x-order"]
            .as_u64()
            .unwrap();
        assert!(
            chat < docs,
            "chat completions must sort before the docs endpoints"
        );
        assert_eq!(chat, 0, "chat completions leads the catalog");
    }

    #[test]
    fn one_path_with_two_methods_becomes_two_operations() {
        // PUT and DELETE share /api/config/providers/{name}; a naive renderer would
        // have the second overwrite the first.
        let spec = openapi(BASE);
        let item = &spec["paths"]["/api/config/providers/{name}"];
        assert!(item["put"].is_object(), "PUT lost");
        assert!(item["delete"].is_object(), "DELETE lost");
    }

    #[test]
    fn public_endpoints_carry_no_security_requirement() {
        let spec = openapi(BASE);
        assert!(
            spec["paths"]["/health"]["get"]["security"].is_null(),
            "health must not demand a credential"
        );
        assert!(
            spec["paths"]["/api/logs"]["get"]["security"].is_array(),
            "control-plane routes must declare bearer auth"
        );
    }

    #[test]
    fn body_params_become_a_request_body_with_required_fields() {
        let spec = openapi(BASE);
        let schema = &spec["paths"]["/v1/chat/completions"]["post"]["requestBody"]["content"]
            ["application/json"]["schema"];
        assert!(schema["properties"]["model"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "model"));
        assert!(
            !required.iter().any(|v| v == "stream"),
            "optional fields must not be marked required"
        );
    }

    #[test]
    fn llms_txt_follows_the_index_convention() {
        let s = llms_txt(BASE);
        assert!(s.starts_with("# KGateway\n"), "H1 title first");
        assert!(s.contains("\n> "), "blockquote summary after the title");
        // Link per endpoint, pointing at a fetchable .md twin.
        for e in ENDPOINTS {
            let link = format!("]({BASE}/docs/{}.md)", e.slug());
            assert!(s.contains(&link), "missing index link for {}", e.path);
        }
        assert!(s.contains("/llms-full.txt"), "points at the full version");
    }

    #[test]
    fn every_llms_txt_link_resolves_to_a_real_endpoint() {
        // A broken link in the index sends an agent to a 404 and it gives up on the rest.
        for line in llms_txt(BASE).lines().filter(|l| l.starts_with("- [")) {
            // Parse the link TARGET, not the display text — one entry's display text
            // is itself a /docs/ path, which a naive search matches first.
            let Some(open) = line.find("](") else {
                continue;
            };
            let target = &line[open + 2..];
            let Some(close) = target.find(')') else {
                continue;
            };
            let target = &target[..close];
            let Some(start) = target.rfind("/docs/") else {
                continue;
            };
            let slug = target[start + "/docs/".len()..].trim_end_matches(".md");
            assert!(
                endpoint_by_slug(slug).is_some(),
                "index links to /docs/{slug}.md which resolves to nothing"
            );
        }
    }

    #[test]
    fn markdown_page_carries_auth_params_and_a_runnable_example() {
        let e = endpoint_by_slug("post-v1-chat-completions").unwrap();
        let md = endpoint_markdown(e, BASE);
        assert!(md.starts_with("# POST /v1/chat/completions"));
        assert!(md.contains("**Auth:**"));
        assert!(md.contains("| `model` |"), "parameter table rendered");
        assert!(md.contains("```bash"), "example is a fenced code block");
        assert!(md.contains("curl "), "example is runnable");
    }

    #[test]
    fn base_url_is_substituted_into_examples() {
        let e = endpoint_by_slug("get-health").unwrap();
        let md = endpoint_markdown(e, "https://gw.example.com");
        assert!(md.contains("https://gw.example.com/health"));
        assert!(
            !md.contains("localhost:8080"),
            "examples should target the caller's own gateway"
        );
    }

    #[test]
    fn llms_full_inlines_every_endpoint() {
        let s = llms_full_txt(BASE);
        for e in ENDPOINTS {
            assert!(
                s.contains(&format!("# {} {}", e.method, e.path)),
                "{} {} not inlined",
                e.method,
                e.path
            );
        }
        // Worth knowing if this balloons: it is meant to fit in a model's context.
        assert!(
            s.len() < 200_000,
            "llms-full.txt grew to {} bytes — too large to paste into a context",
            s.len()
        );
    }
}
