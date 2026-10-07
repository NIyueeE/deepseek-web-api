//! 统一协议调试 CLI
//!
//! 接受 OpenAI JSON 请求体，支持输出原始 DeepSeek SSE、转换后的 OpenAI SSE 或两者对照。
//!
//! 使用方式:
//!   交互模式: cargo run --example adapter_cli
//!   脚本模式: cargo run --example adapter_cli -- source examples/adapter_cli-script.txt
//!
//! 命令:
//!   chat <json_file>                       - OpenAI 转换后输出
//!   raw <json_file>                        - 原始 DeepSeek SSE（转换前）
//!   compare <json_file>                    - 上下对照两种流
//!   concurrent <n> <json_file>             - 并发请求
//!   models                                 - 列出可用模型
//!   model <id>                             - 查询单个模型
//!   status                                 - 查看账号池状态
//!   source <file>                          - 从文件读取命令执行
//!   quit | exit                            - 退出并清理

use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use ds_free_api::{ChatCompletionsRequest, ChatOutput, Config, OpenAIAdapter, StreamResponse};
use futures::{StreamExt, future::join_all};
use std::io::{self, Read, Write};
use std::path::Path;

static DEMO_COUNTER: AtomicU64 = AtomicU64::new(0);

fn demo_req_id() -> String {
    format!("demo-{:x}", DEMO_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn read_line_lossy() -> io::Result<String> {
    let mut buf = Vec::new();
    let mut handle = io::stdin().lock();
    loop {
        let mut byte = [0u8; 1];
        match handle.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if byte[0] != b'\r' {
                    buf.push(byte[0]);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::new().default_filter_or("info")).init();

    let (config, _config_path) = Config::load_with_args(std::env::args())?;
    println!("[初始化中...]");
    let adapter = OpenAIAdapter::new(&config).await?;
    println!(
        "[就绪] 命令: chat | raw | compare | concurrent | models | model | status | source | quit"
    );

    let mut stdout = io::stdout();

    loop {
        print!("> ");
        stdout.flush()?;

        let line = read_line_lossy()?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if handle_line(line, &adapter).await? {
            break;
        }
    }

    println!("[清理中...]");
    adapter.shutdown().await;
    println!("[已关闭]");

    Ok(())
}

fn parse_args<'a>(parts: &'a [&'a str]) -> (Vec<&'a str>, bool) {
    let raw = parts.iter().any(|p| *p == "--raw" || *p == "-r");
    let positional: Vec<_> = parts
        .iter()
        .filter(|p| **p != "--raw" && **p != "-r")
        .copied()
        .collect();
    (positional, raw)
}

async fn handle_line(line: &str, adapter: &OpenAIAdapter) -> anyhow::Result<bool> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(false);
    }

    let cmd = parts[0];
    match cmd {
        "status" => {
            println!("[账号状态]");
            for (i, s) in adapter.account_statuses().iter().enumerate() {
                let email = if s.email.is_empty() { "-" } else { &s.email };
                let mobile = if s.mobile.is_empty() { "-" } else { &s.mobile };
                println!("  [{}] {} / {}", i + 1, email, mobile);
            }
        }

        "chat" if parts.len() >= 2 => {
            let file = parts[1];
            let body = load_json(file)?;
            let rid = demo_req_id();
            println!(">>> chat: {file} [req={rid}]");
            let req = serde_json::from_slice::<ChatCompletionsRequest>(&body)?;
            let result = adapter.chat_completions(req, &rid).await?;
            println!("[account: {}]", result.account_id);
            match result.data {
                ChatOutput::Stream(mut s) => {
                    use futures::StreamExt;
                    // ChunkStream → print each chunk as JSON line
                    while let Some(chunk) = s.next().await {
                        match chunk {
                            Ok(c) => println!("{}", serde_json::to_string(&c).unwrap()),
                            Err(e) => eprintln!("流错误: {e}"),
                        }
                    }
                }
                ChatOutput::Json(json) => println!("{}", serde_json::to_string(&json).unwrap()),
            }
        }

        "raw" if parts.len() >= 2 => {
            let file = parts[1];
            let body = load_json(file)?;
            let rid = demo_req_id();
            println!(">>> raw: {file} [req={rid}]");
            let mut result = adapter.raw_chat_completions_stream(&body, &rid).await?;
            println!("[account: {}]", result.account_id);
            print_stream(&mut result.data, true).await;
        }

        "compare" if parts.len() >= 2 => {
            let file = parts[1];
            let body = load_json(file)?;
            println!(">>> compare: {file}");

            // 原始流
            let rid1 = demo_req_id();
            println!("\n═══ RAW DEEPSEEK SSE [req={rid1}] ═════════════════════════════════");
            let raw_result = adapter.raw_chat_completions_stream(&body, &rid1).await?;
            println!("[account: {}]", raw_result.account_id);
            consume_stream(raw_result.data, |bytes| {
                let text = String::from_utf8_lossy(&bytes);
                for line in text.lines() {
                    println!("  {line}");
                }
            })
            .await;

            // 转换后流
            let rid2 = demo_req_id();
            println!("\n═══ CONVERTED OPENAI SSE [req={rid2}] ═════════════════════════════");
            let conv_req = serde_json::from_slice::<ChatCompletionsRequest>(&body)?;
            let converted_result = adapter.chat_completions(conv_req, &rid2).await?;
            println!("[account: {}]", converted_result.account_id);
            match converted_result.data {
                ChatOutput::Stream(mut s) => {
                    use futures::StreamExt;
                    while let Some(chunk) = s.next().await {
                        match chunk {
                            Ok(c) => println!("  {}", serde_json::to_string(&c).unwrap()),
                            Err(e) => eprintln!("流错误: {e}"),
                        }
                    }
                }
                ChatOutput::Json(_) => {}
            }

            println!("\n═══ END ════════════════════════════════════════════════════");
        }

        "concurrent" if parts.len() >= 3 => {
            let (positional, raw) = parse_args(&parts);
            let count: usize = match positional[1].parse() {
                Ok(n) if n > 0 => n,
                _ => {
                    eprintln!("[错误] 并发数必须是正整数");
                    return Ok(false);
                }
            };
            let file = positional[2];
            let body = load_json(file)?;
            println!(">>> concurrent: count={count}, file={file}");
            run_concurrent(adapter, count, body, raw).await;
        }

        "models" => {
            let list = adapter.list_models().await;
            println!("{}", serde_json::to_string(&list).unwrap());
        }

        "model" if parts.len() == 2 => {
            if let Some(model) = adapter.get_model(parts[1]).await {
                println!("{}", serde_json::to_string(&model).unwrap());
            } else {
                println!("null");
            }
        }

        "source" if parts.len() == 2 => {
            let file = parts[1];
            if !Path::new(file).exists() {
                eprintln!("[错误] 文件不存在: {file}");
                return Ok(false);
            }
            println!("[执行脚本: {file}]");
            let content = std::fs::read_to_string(file)?;
            for script_line in content.lines() {
                let script_line = script_line.trim();
                if script_line.is_empty() || script_line.starts_with('#') {
                    continue;
                }
                println!(">>> {script_line}");
                if Box::pin(handle_line(script_line, adapter)).await? {
                    return Ok(true);
                }
            }
            println!("[脚本执行完毕]");
        }

        "quit" | "exit" => {
            println!("[退出]");
            return Ok(true);
        }

        _ => {
            println!(
                "[未知命令: {cmd}] 可用: chat | raw | compare | concurrent | models | model | status | source | quit"
            );
        }
    }

    Ok(false)
}

fn load_json(file: &str) -> anyhow::Result<Vec<u8>> {
    let path = Path::new(file);
    if !path.exists() {
        anyhow::bail!("文件不存在: {file}");
    }
    Ok(std::fs::read(path)?)
}

/// 消费流并给每个 chunk 应用处理函数
async fn consume_stream<F>(stream: StreamResponse, mut f: F)
where
    F: FnMut(Bytes),
{
    let mut stream = stream;
    while let Some(res) = stream.next().await {
        match res {
            Ok(bytes) => f(bytes),
            Err(e) => {
                eprintln!("\n[流错误] {e}");
                break;
            }
        }
    }
}

/// 打印流式响应
async fn print_stream(stream: &mut StreamResponse, raw: bool) {
    let mut stdout = io::stdout();
    while let Some(res) = stream.next().await {
        match res {
            Ok(bytes) => {
                if raw {
                    print!("{}", String::from_utf8_lossy(&bytes));
                    stdout.flush().unwrap();
                } else {
                    print_stream_chunk(&bytes);
                }
            }
            Err(e) => {
                eprintln!("\n[流错误] {e}");
                break;
            }
        }
    }
    if !raw {
        println!();
    }
}

/// 打印单个转换后 chunk 的摘要
fn print_stream_chunk(bytes: &Bytes) {
    let text = String::from_utf8_lossy(bytes);
    let json_str = text
        .strip_prefix("data: ")
        .and_then(|s| s.strip_suffix("\n\n"))
        .unwrap_or(&text);

    let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) else {
        print!("{text}");
        return;
    };

    let choice = v.get("choices").and_then(|c| c.get(0));
    let delta = choice.and_then(|c| c.get("delta"));
    let content = delta
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_str());
    let reasoning = delta
        .and_then(|d| d.get("reasoning_content"))
        .and_then(|c| c.as_str());
    let tool_calls = delta.and_then(|d| d.get("tool_calls"));
    let finish = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str());
    let usage = v.get("usage");

    if (choice.is_none() || usage.is_some())
        && let Some(u) = usage
    {
        println!("[usage] {u}");
        return;
    }

    let mut parts = Vec::new();
    if let Some(c) = content {
        parts.push(format!("content={c:?}"));
    }
    if let Some(r) = reasoning {
        parts.push(format!("reasoning={r:?}"));
    }
    if let Some(t) = tool_calls {
        parts.push(format!(
            "tool_calls={}",
            serde_json::to_string(t).unwrap_or_default()
        ));
    }
    if let Some(f) = finish {
        parts.push(format!("finish={f}"));
    }

    if !parts.is_empty() {
        println!("[chunk] {}", parts.join(" | "));
    }
}

/// 执行并发请求
async fn run_concurrent(adapter: &OpenAIAdapter, count: usize, body: Vec<u8>, raw: bool) {
    let start = std::time::Instant::now();

    let futures: Vec<_> = (0..count)
        .map(|i| {
            let body = body.clone();
            async move {
                let req_start = std::time::Instant::now();
                let rid = demo_req_id();

                let req = match serde_json::from_slice::<ChatCompletionsRequest>(&body) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[请求{i} 解析失败] {e}");
                        return (i, false, String::new(), req_start.elapsed());
                    }
                };

                let result = match adapter.chat_completions(req, &rid).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[请求{i} 失败] {e}");
                        return (i, false, String::new(), req_start.elapsed());
                    }
                };

                let (ok, output) = match result.data {
                    ChatOutput::Stream(mut s) => {
                        use futures::StreamExt;
                        let mut output = String::new();
                        let mut ok = true;
                        while let Some(chunk) = s.next().await {
                            match chunk {
                                Ok(c) => {
                                    if raw {
                                        output.push_str(&serde_json::to_string(&c).unwrap());
                                    } else if let Some(choice) = c.choices.first() {
                                        if let Some(ref content) = choice.delta.content {
                                            output.push_str(content);
                                        }
                                        if let Some(ref reasoning) = choice.delta.reasoning_content
                                        {
                                            if !output.is_empty() {
                                                output.push(' ');
                                            }
                                            output.push_str(reasoning);
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("\n[请求{i} 流错误] {e}");
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        (ok, output)
                    }
                    ChatOutput::Json(json) => {
                        let output = if raw {
                            serde_json::to_string(&json).unwrap_or_default()
                        } else {
                            let mut parts = Vec::new();
                            if let Some(c) = json
                                .choices
                                .first()
                                .and_then(|c| c.message.content.as_deref())
                            {
                                parts.push(c.to_string());
                            }
                            if let Some(r) = json
                                .choices
                                .first()
                                .and_then(|c| c.message.reasoning_content.as_deref())
                            {
                                parts.push(r.to_string());
                            }
                            parts.join(" ")
                        };
                        (true, output)
                    }
                };
                (i, ok, output, req_start.elapsed())
            }
        })
        .collect();

    let results = join_all(futures).await;
    let total_elapsed = start.elapsed();

    println!("\n[并发结果]");
    let success_count = results.iter().filter(|(_, ok, _, _)| *ok).count();
    for (i, ok, output, elapsed) in results {
        let preview: String = output.chars().take(80).collect();
        let status = if ok { "成功" } else { "失败" };
        println!(
            "  [请求{:2}] {} | {:>12?} | {}",
            i,
            status,
            elapsed,
            if preview.is_empty() {
                "(无输出)".to_string()
            } else {
                format!("{}...", output.replace('\n', " "))
            }
        );
    }
    println!("  总计: {success_count}/{count} 成功 | 总耗时 {total_elapsed:?}");
}
