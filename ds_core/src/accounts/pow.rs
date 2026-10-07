//! PoW 计算器 —— 基于 DeepSeek WASM 的 DeepSeekHashV1 算法实现
//!
//! 通过签名动态探测 wasm-bindgen 导出符号，避免硬编码 __wbindgen_export_0 导致
//! DeepSeek 更新 WASM 后无法启动。

use wasmtime::{AsContextMut, Engine, InstancePre, Linker, Module, Store, ValType};

// 复用 client 的 ChallengeData，避免重复定义
pub use super::client::ChallengeData as Challenge;

#[derive(Clone)]
pub struct PowSolver {
    engine: Engine,
    instance_pre: InstancePre<()>,
    add_to_stack_name: String,
    alloc_name: String,
    solve_name: String,
}

#[derive(Debug)]
pub struct PowResult {
    pub algorithm: String,
    pub challenge: String,
    pub salt: String,
    pub answer: i64,
    pub signature: String,
    pub target_path: String,
}

impl PowResult {
    /// 将 PoW 结果转换为 base64 编码的 header
    pub fn to_header(&self) -> String {
        let json = serde_json::json!({
            "algorithm": self.algorithm,
            "challenge": self.challenge,
            "salt": self.salt,
            "answer": self.answer,
            "signature": self.signature,
            "target_path": self.target_path,
        });
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            json.to_string().as_bytes(),
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PowError {
    #[error("WASM init failed: {0}")]
    WasmInit(String),
    #[error("WASM solve failed: no solution")]
    NoSolution,
    #[error("unsupported algorithm: {0}")]
    UnsupportedAlgorithm(String),
    #[error("WASM execution error: {0}")]
    Execution(String),
}

impl PowSolver {
    pub fn new(wasm_bytes: &[u8]) -> Result<Self, PowError> {
        let engine = Engine::default();
        let module =
            Module::new(&engine, wasm_bytes).map_err(|e| PowError::WasmInit(e.to_string()))?;
        let linker = Linker::new(&engine);
        let instance_pre = linker
            .instantiate_pre(&module)
            .map_err(|e| PowError::WasmInit(e.to_string()))?;

        let exports: Vec<_> = module.exports().collect();

        let add_to_stack_name = find_export_by_names(
            &exports,
            &["__wbindgen_add_to_stack_pointer"],
            &[ValType::I32],
            &[ValType::I32],
        )
        .ok_or_else(|| {
            PowError::WasmInit("__wbindgen_add_to_stack_pointer not found".to_string())
        })?;

        // allocator: 优先找 __wbindgen_malloc，其次是签名匹配的 __wbindgen_export_*
        let alloc_name = find_export_by_names(
            &exports,
            &["__wbindgen_malloc"],
            &[ValType::I32, ValType::I32],
            &[ValType::I32],
        )
        .or_else(|| {
            find_export_by_prefix(
                &exports,
                "__wbindgen_export_",
                &[ValType::I32, ValType::I32],
                &[ValType::I32],
            )
        })
        .ok_or_else(|| PowError::WasmInit("allocator export not found".to_string()))?;

        // wasm_solve: 优先显式名称，再按唯一签名 (i32, i32, i32, i32, i32, f64) -> () 探测
        let solve_name = find_export_by_names(
            &exports,
            &["wasm_solve"],
            &[
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::F64,
            ],
            &[],
        )
        .or_else(|| {
            let candidates: Vec<_> = exports
                .iter()
                .filter(|e| {
                    matches_sig(
                        e,
                        &[
                            ValType::I32,
                            ValType::I32,
                            ValType::I32,
                            ValType::I32,
                            ValType::I32,
                            ValType::F64,
                        ],
                        &[],
                    )
                })
                .map(|e| e.name().to_string())
                .collect();
            (candidates.len() == 1).then(|| candidates.into_iter().next().unwrap())
        })
        .ok_or_else(|| PowError::WasmInit("wasm_solve export not found".to_string()))?;

        Ok(Self {
            engine,
            instance_pre,
            add_to_stack_name,
            alloc_name,
            solve_name,
        })
    }

    pub fn solve(&self, challenge: &Challenge) -> Result<PowResult, PowError> {
        if challenge.algorithm != "DeepSeekHashV1" {
            return Err(PowError::UnsupportedAlgorithm(challenge.algorithm.clone()));
        }

        let mut store = Store::new(&self.engine, ());

        let instance = self
            .instance_pre
            .instantiate(&mut store)
            .map_err(|e| PowError::Execution(e.to_string()))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| PowError::Execution("memory not found".to_string()))?;
        let add_to_stack = instance
            .get_typed_func::<i32, i32>(&mut store, &self.add_to_stack_name)
            .map_err(|e| PowError::Execution(e.to_string()))?;
        let alloc = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, &self.alloc_name)
            .map_err(|e| PowError::Execution(e.to_string()))?;
        let wasm_solve = instance
            .get_typed_func::<(i32, i32, i32, i32, i32, f64), ()>(&mut store, &self.solve_name)
            .map_err(|e| PowError::Execution(e.to_string()))?;

        let prefix = format!("{}_{}_", challenge.salt, challenge.expire_at);

        let retptr = add_to_stack
            .call(&mut store, -16)
            .map_err(|e| PowError::Execution(e.to_string()))?;

        let (ptr_challenge, len_challenge) =
            write_string(&mut store, &memory, &alloc, &challenge.challenge)?;
        let (ptr_prefix, len_prefix) = write_string(&mut store, &memory, &alloc, &prefix)?;

        // WASM 的 solve 接收 JS number；difficulty 由上游下发且远小于 2^53，
        // 用 u32 中转即可无损转成 f64。越界值按 u32::MAX 处理，最终以 NoSolution 收敛。
        let difficulty = u32::try_from(challenge.difficulty).map_or_else(
            |_| {
                log::warn!(
                    target: "ds_core::client",
                    "PoW difficulty 越界: {}",
                    challenge.difficulty
                );
                f64::from(u32::MAX)
            },
            f64::from,
        );

        wasm_solve
            .call(
                &mut store,
                (
                    retptr,
                    ptr_challenge,
                    len_challenge,
                    ptr_prefix,
                    len_prefix,
                    difficulty,
                ),
            )
            .map_err(|e| PowError::Execution(e.to_string()))?;

        let mut status_buf = [0u8; 4];
        memory
            .read(&mut store, retptr.cast_unsigned() as usize, &mut status_buf)
            .map_err(|e| PowError::Execution(e.to_string()))?;
        let status = i32::from_le_bytes(status_buf);

        let mut value_buf = [0u8; 8];
        memory
            .read(
                &mut store,
                (retptr + 8).cast_unsigned() as usize,
                &mut value_buf,
            )
            .map_err(|e| PowError::Execution(e.to_string()))?;
        let value = f64::from_le_bytes(value_buf);

        if status == 0 {
            return Err(PowError::NoSolution);
        }

        Ok(PowResult {
            algorithm: challenge.algorithm.clone(),
            challenge: challenge.challenge.clone(),
            salt: challenge.salt.clone(),
            answer: parse_answer(value)?,
            signature: challenge.signature.clone(),
            target_path: challenge.target_path.clone(),
        })
    }
}

// ── WASM 辅助函数 ──────────────────────────────────────────────────────

fn write_string(
    store: &mut impl AsContextMut,
    memory: &wasmtime::Memory,
    alloc: &wasmtime::TypedFunc<(i32, i32), i32>,
    s: &str,
) -> Result<(i32, i32), PowError> {
    let bytes = s.as_bytes();
    // WASM 侧长度参数是 i32：超长直接报错，避免无声截断
    let len = i32::try_from(bytes.len()).map_err(|_| {
        PowError::Execution(format!(
            "字符串过长无法写入 WASM 内存: {} 字节",
            bytes.len()
        ))
    })?;
    let ptr = alloc
        .call(store.as_context_mut(), (len, 1))
        .map_err(|e| PowError::Execution(e.to_string()))?;
    memory
        .write(store.as_context_mut(), ptr.cast_unsigned() as usize, bytes)
        .map_err(|e| PowError::Execution(e.to_string()))?;
    Ok((ptr, len))
}

/// 将 WASM 返回的答案转成 i64
///
/// WASM 侧返回的是 f64 整数。直接 `as i64` 在越界/NaN 时会静默饱和，
/// 这里显式截断小数部分并在越界时报错 —— PoW 求解开销远大于一次格式化。
fn parse_answer(value: f64) -> Result<i64, PowError> {
    if !value.is_finite() {
        return Err(PowError::Execution(format!("WASM 返回非有限答案: {value}")));
    }
    value
        .trunc()
        .to_string()
        .parse()
        .map_err(|_| PowError::Execution(format!("WASM 答案超出 i64 范围: {value}")))
}

fn find_export_by_names(
    exports: &[wasmtime::ExportType<'_>],
    names: &[&str],
    params: &[ValType],
    results: &[ValType],
) -> Option<String> {
    exports
        .iter()
        .find(|e| names.contains(&e.name()) && matches_sig(e, params, results))
        .map(|e| e.name().to_string())
}

fn find_export_by_prefix(
    exports: &[wasmtime::ExportType<'_>],
    prefix: &str,
    params: &[ValType],
    results: &[ValType],
) -> Option<String> {
    exports
        .iter()
        .find(|e| e.name().starts_with(prefix) && matches_sig(e, params, results))
        .map(|e| e.name().to_string())
}

fn matches_sig(e: &wasmtime::ExportType<'_>, params: &[ValType], results: &[ValType]) -> bool {
    let ext_ty = e.ty();
    let Some(func_ty) = ext_ty.func() else {
        return false;
    };
    let p: Vec<_> = func_ty.params().collect();
    let r: Vec<_> = func_ty.results().collect();
    p.len() == params.len()
        && r.len() == results.len()
        && p.iter()
            .zip(params.iter())
            .all(|(a, b)| std::mem::discriminant(a) == std::mem::discriminant(b))
        && r.iter()
            .zip(results.iter())
            .all(|(a, b)| std::mem::discriminant(a) == std::mem::discriminant(b))
}
