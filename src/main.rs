use serde::Deserialize;
use serde_json::{json, Value};
use std::error::Error;
use tokio::io::{self, AsyncBufReadExt, BufReader};

const SEARXNG_URL: &str = "http://127.0.0.1:8888/search";

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    method: String,
    id: Option<Value>,
    params: Option<Value>,
}

#[derive(Deserialize)]
struct CallToolParams {
    name: String,
    arguments: Option<Value>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin).lines();

    // Change: Use standard log streaming to stderr so llama-server doesn't misinterpret it as JSON
    eprintln!("Server started. Waiting for input...");

    while let Some(line) = reader.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Change: Since MCP clients emit one complete JSON object per line, parse line directly
        // to avoid multi-line accumulator synchronization gaps.
        if let Ok(req) = serde_json::from_str::<JsonRpcRequest>(trimmed) {
            let response = match req.method.as_str() {
                "initialize" => json!({
                    "jsonrpc": "2.0",
                    "id": req.id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {
                            "tools": { "listChanged": false }
                        },
                        "serverInfo": { "name": "rust-mcp-agent", "version": "1.2.0" }
                    }
                }),

                // FIX: Modified payload format to match strict Model Context Protocol spec requirements
                "tools/list" => json!({
                    "jsonrpc": "2.0",
                    "id": req.id,
                    "result": {
                        "tools": [
                            {
                                "name": "search_encyclopedia",
                                "description": "Fetches facts from DuckDuckGo.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": { "query": { "type": "string" } },
                                    "required": ["query"]
                                }
                            },
                            {
                                "name": "search_web",
                                "description": "Live search via local SearXNG.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": { "query": { "type": "string" } },
                                    "required": ["query"]
                                }
                            },
                            {
                                "name": "fetch_url",
                                "description": "Scrapes a URL and returns Markdown.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": { "url": { "type": "string" } },
                                    "required": ["url"]
                                }
                            }
                        ]
                    }
                }),

                "tools/call" => {
                    let params_val = match &req.params {
                        Some(p) => p,
                        None => {
                            println!("{}", json!({"jsonrpc": "2.0", "id": req.id, "error": { "code": -32602, "message": "Missing parameters" }}));
                            continue;
                        }
                    };

                    let tool: CallToolParams = match serde_json::from_value(params_val.clone()) {
                        Ok(t) => t,
                        Err(_) => {
                            println!("{}", json!({"jsonrpc": "2.0", "id": req.id, "error": { "code": -32602, "message": "Invalid arguments format" }}));
                            continue;
                        }
                    };

                    let query = args_to_str(&tool.arguments, "query");
                    let url = args_to_str(&tool.arguments, "url");

                    let res_content = match tool.name.as_str() {
                        "search_encyclopedia" => execute_ddg_api(query).await.map_err(|e| e.to_string()),
                        "search_web" => execute_searxng(query).await.map_err(|e| e.to_string()),
                        "fetch_url" => scrape_page(url).await.map_err(|e| e.to_string()),
                        _ => Err("Tool not found".to_string()),
                    };

                    match res_content {
                        Ok(content) => json!({
                            "jsonrpc": "2.0",
                            "id": req.id,
                            "result": {
                                "content": [{ "type": "text", "text": content }],
                                "isError": false
                            }
                        }),
                        Err(e) => json!({
                            "jsonrpc": "2.0",
                            "id": req.id,
                            "result": {
                                "content": [{ "type": "text", "text": e }],
                                "isError": true
                            }
                        }),
                    }
                }

                _ => if req.id.is_some() {
                    json!({ "jsonrpc": "2.0", "id": req.id, "error": { "code": -32601, "message": "Method not found" } })
                } else { 
                    continue 
                },
            };

            // Ensure response outputs cleanly on a single line back to llama-server
            println!("{}", serde_json::to_string(&response)?);
        }
    }
    Ok(())
}

fn args_to_str(args: &Option<Value>, key: &str) -> String {
    args.as_ref().and_then(|a| a.get(key)).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

// --- Core Engines ---

async fn execute_ddg_api(query: String) -> Result<String, Box<dyn Error>> {
    let client = reqwest::Client::builder().user_agent("Mozilla/5.0").build()?;
    let url = format!("https://api.duckduckgo.com/?q={}&format=json&no_html=1", urlencoding::encode(&query));
    let res = client.get(&url).send().await?.json::<Value>().await?;
    
    let mut out = format!("# Encyclopedia Results for: {}\n\n", query);

    match res.get("AbstractText").and_then(|v| v.as_str()) {
        Some(text) if !text.trim().is_empty() => {
            let abstract_url = res.get("AbstractURL").and_then(|v| v.as_str()).unwrap_or("#");
            out.push_str(&format!("**{}**\n   *URL:* {}\n\n", text, abstract_url));
        }
        _ => {
            out.push_str("No specific encyclopedia summary was found. Please try a more specific term.\n\n");
        }
    }
    Ok(out)
}

async fn execute_searxng(query: String) -> Result<String, Box<dyn Error>> {
    let client = reqwest::Client::builder().user_agent("LlamaMcpAgent/1.0").build()?;
    let resp = client.get(SEARXNG_URL).query(&[("q", query.as_str()), ("format", "json")]).send().await?;
    
    if !resp.status().is_success() { return Err(format!("SearXNG error: {}", resp.status()).into()); }
    
    let json_data: Value = resp.json().await?;
    let mut out = format!("# Live Web Index Search Results for: {}\n\n", query);
    if let Some(results) = json_data.get("results").and_then(|r| r.as_array()) {
        for item in results.iter().take(5) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
            let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
            out.push_str(&format!("**{}**\n   *Summary:* {}\n\n", title, content));
        }
    }
    Ok(out)
}

async fn scrape_page(url: String) -> Result<String, Box<dyn Error>> {
    let client = reqwest::Client::builder().user_agent("LlamaMcpScraper/1.0").build()?;
    let body = client.get(&url).send().await?.text().await?;
    let cleaned = remove_blocks(&remove_blocks(&body, "script"), "style");
    Ok(html2md::parse_html(&cleaned))
}

fn remove_blocks(html: &str, tag: &str) -> String {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut out = String::new();
    let mut pos = 0;
    while let Some(start) = html[pos..].find(&open) {
        let abs_start = pos + start;
        out.push_str(&html[pos..abs_start]);
        if let Some(end) = html[abs_start..].find(&close) {
            pos = abs_start + end + close.len();
        } else { 
            pos = html.len(); 
            break; 
        }
    }
    out.push_str(&html[pos..]);
    out
}

// {"jsonrpc":"2.0","method":"tools/call","id":3,"params":{"name":"search_encyclopedia","arguments":{"query":"Mars"}}}
