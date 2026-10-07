const TOKEN_KEY = 'ds-admin-token';

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}

export function setToken(token: string) {
  localStorage.setItem(TOKEN_KEY, token);
}
export function clearToken() {
  localStorage.removeItem(TOKEN_KEY);
}

let onUnauthorized: (() => void) | null = null;

/** Register 401 callback: automatically invoked upon 401 response (for AuthProvider token synchronization) */
export function setOnUnauthorized(cb: (() => void) | null) {
  onUnauthorized = cb;
}

export async function apiFetch<T>(path: string, init?: RequestInit): Promise<T> {
  const token = getToken();
  const headers: Record<string, string> = {
    'Accept': 'application/json',
    'Content-Type': 'application/json',
    ...(init?.headers as Record<string, string> ?? {}),
  };
  if (token) {
    headers['Authorization'] = `Bearer ${token}`;
  }

  const res = await fetch(path, { ...init, headers });
  if (res.status === 401) {
    clearToken();
    onUnauthorized?.();
    throw new AuthError('Unauthorized');
  }
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new ApiError(res.status, body.error || `API error: ${res.status}`);
  }
  return res.json();
}

export class AuthError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'AuthError';
  }
}

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

// ── Auth API ──────────────────────────────────────────────────────────────

export interface LoginResponse {
  token: string;
}

export async function apiSetup(password: string): Promise<LoginResponse> {
  return apiFetch<LoginResponse>('/admin/api/setup', {
    method: 'POST',
    body: JSON.stringify({ password }),
  });
}

export async function apiLogin(password: string): Promise<LoginResponse> {
  return apiFetch<LoginResponse>('/admin/api/login', {
    method: 'POST',
    body: JSON.stringify({ password }),
  });
}

// ── Data Types ────────────────────────────────────────────────────────────

export interface RequestLog {
  timestamp: number;
  request_id: string;
  model: string;
  api_key: string;
  prompt_tokens: number;
  completion_tokens: number;
  latency_ms: number;
  success: boolean;
}

export interface RuntimeLogEntry {
  timestamp: string;
  level: string;
  target: string;
  message: string;
}

export interface RuntimeLogsResponse {
  total: number;
  offset: number;
  limit: number;
  logs: RuntimeLogEntry[];
}

export interface AccountStatus {
  email: string;
  mobile: string;
  state: string;
  last_released_ms: number;
  error_count: number;
  /** Current-hour request usage for this account */
  used_this_hour: number;
  /** True when the hourly quota is used up (quota 0 = unlimited, always false) */
  quota_exhausted: boolean;
}

export interface AdminStatusResponse {
  accounts: AccountStatus[];
  total: number;
  idle: number;
  busy: number;
  error: number;
  invalid: number;
}

export interface StatsSnapshot {
  total_requests: number;
  success_requests: number;
  failed_requests: number;
  avg_latency_ms: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  uptime_secs: number;
  models: Record<string, { prompt_tokens: number; completion_tokens: number; requests: number }>;
  keys: Record<string, { prompt_tokens: number; completion_tokens: number; requests: number }>;
}

export interface ModelInfo {
  id: string;
  object: string;
  created: number;
  owned_by: string;
  max_input_tokens?: number;
  max_output_tokens?: number;
  context_length?: number;
  context_window?: number;
  max_tokens?: number;
  max_completion_tokens?: number;
}

export interface ModelListResponse {
  object: string;
  data: ModelInfo[];
}

// ── Config Types (mirrors backend response) ───────────────────────────────

export interface ServerConfig {
  host: string;
  port: number;
  cors_origins: string[];
}

export interface ToolCallTagConfig {
  extra_starts: string[];
  extra_ends: string[];
}

export interface AccountEntry {
  email: string;
  mobile: string;
  area_code: string;
  password: string;
  device_id?: string;
}

export interface DsCoreConfig {
  accounts: AccountEntry[];
  api_base: string;
  wasm_url: string;
  user_agent: string;
  client_version: string;
  client_platform: string;
  client_locale: string;
  client_os: string;
  client_bundle_id: string;
  client_device_id: string;
  client_device_model: string;
  client_timezone_offset: string;
  emulation: string;
  hif_enabled: boolean;
  model_types: string[];
  max_input_tokens: number[];
  max_output_tokens: number[];
  input_character_limits: number[];
  model_aliases: string[];
  tool_call: ToolCallTagConfig;
  default_search_enabled: boolean;
  hourly_request_quota: number;
  responses_store_capacity: number;
  responses_store_ttl_secs: number;
}

export interface ProxyConfig {
  url: string | null;
}

export interface AdminConfigResponse {
  password_set: boolean;
  jwt_issued_at: number;
}

export interface ApiKeyEntry {
  key: string;
  description: string;
}

export interface FullConfig {
  server: ServerConfig;
  ds_core: DsCoreConfig;
  proxy: ProxyConfig;
  admin: AdminConfigResponse;
  api_keys: ApiKeyEntry[];
}

/**
 * 后端默认值（必须与 src/config.rs 的 default_* 函数保持一致）。
 * 前端在这里兜底，避免缺字段时把非法配置写回后端。
 */
const DEFAULTS = {
  apiBase: 'https://chat.deepseek.com/api/v0',
  userAgent: 'DeepSeek/2.5.0 Android/35',
  clientVersion: '2.5.0',
  clientPlatform: 'android',
  clientLocale: 'zh_CN',
  clientOs: 'android',
  clientBundleId: 'com.deepseek.chat',
  clientDeviceId: '',
  clientDeviceModel: '',
  clientTimezoneOffset: '28800',
  emulation: 'okhttp4_12',
  hifEnabled: true,
  maxInputTokens: 1048576,
  maxOutputTokens: 384000,
  maxChars: 2621440,
  responsesStoreCapacity: 256,
  responsesStoreTtlSecs: 3600,
  defaultSearchEnabled: true,
  hourlyRequestQuota: 60,
} as const;

/**
 * 把按 index 对齐 model_types 的数组补齐/截断到相同长度。
 *
 * 后端 `Config::validate()` 要求 `max_input_tokens` / `max_output_tokens` /
 * `input_character_limits` 与 `model_types` 长度完全一致，
 * 缺失项在这里用默认值补齐。
 */
function align<T>(arr: unknown, len: number, fallback: T): T[] {
  const source = Array.isArray(arr) ? (arr as T[]) : [];
  return Array.from({ length: len }, (_, i) => source[i] ?? fallback);
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function normalizeConfig(raw: any): FullConfig {
  if (!raw) raw = {};
  const accounts = raw.ds_core?.accounts ?? raw.accounts ?? [];
  const core = raw.ds_core ?? raw.deepseek ?? {};

  const modelTypes: string[] = Array.isArray(core.model_types) && core.model_types.length > 0
    ? core.model_types
    : ['default'];
  const len = modelTypes.length;

  return {
    server: {
      host: raw.server?.host ?? '127.0.0.1',
      port: raw.server?.port ?? 22217,
      cors_origins: raw.server?.cors_origins ?? ['http://localhost:22217'],
    },
    ds_core: {
      accounts: Array.isArray(accounts) ? accounts : [],
      api_base: core.api_base ?? DEFAULTS.apiBase,
      wasm_url: core.wasm_url ?? '',
      user_agent: core.user_agent ?? DEFAULTS.userAgent,
      client_version: core.client_version ?? DEFAULTS.clientVersion,
      client_platform: core.client_platform ?? DEFAULTS.clientPlatform,
      client_locale: core.client_locale ?? DEFAULTS.clientLocale,
      client_os: core.client_os ?? DEFAULTS.clientOs,
      client_bundle_id: core.client_bundle_id ?? DEFAULTS.clientBundleId,
      client_device_id: core.client_device_id ?? DEFAULTS.clientDeviceId,
      client_device_model: core.client_device_model ?? DEFAULTS.clientDeviceModel,
      client_timezone_offset: core.client_timezone_offset ?? DEFAULTS.clientTimezoneOffset,
      emulation: core.emulation ?? DEFAULTS.emulation,
      hif_enabled:
        typeof core.hif_enabled === 'boolean' ? core.hif_enabled : DEFAULTS.hifEnabled,
      model_types: modelTypes,
      max_input_tokens: align(core.max_input_tokens, len, DEFAULTS.maxInputTokens),
      max_output_tokens: align(core.max_output_tokens, len, DEFAULTS.maxOutputTokens),
      input_character_limits: align(core.input_character_limits, len, DEFAULTS.maxChars),
      model_aliases: align(core.model_aliases, len, ''),
      tool_call: core.tool_call ?? {
        extra_starts: ['<|tool_call_begin|>', '<tool_calls>', '<tool_call>'],
        extra_ends: ['<|tool_call_end|>', '</tool_calls>', '</tool_call>'],
      },
      default_search_enabled:
        typeof core.default_search_enabled === 'boolean'
          ? core.default_search_enabled
          : DEFAULTS.defaultSearchEnabled,
      hourly_request_quota: core.hourly_request_quota ?? DEFAULTS.hourlyRequestQuota,
      responses_store_capacity:
        core.responses_store_capacity ?? DEFAULTS.responsesStoreCapacity,
      responses_store_ttl_secs:
        core.responses_store_ttl_secs ?? DEFAULTS.responsesStoreTtlSecs,
    },
    proxy: {
      url: raw.proxy?.url ?? null,
    },
    admin: {
      password_set: raw.admin?.password_set ?? true,
      jwt_issued_at: raw.admin?.jwt_issued_at ?? Date.now(),
    },
    api_keys: Array.isArray(raw.api_keys) ? raw.api_keys : [],
  };
}

export async function apiFetchConfig(): Promise<FullConfig> {
  const data = await apiFetch<unknown>('/admin/api/config');
  return normalizeConfig(data);
}

export async function apiSaveConfig(config: Record<string, unknown>): Promise<{ ok: boolean }> {
  return apiFetch<{ ok: boolean }>('/admin/api/config', {
    method: 'PUT',
    body: JSON.stringify(config),
  });
}

// ── Logs ──────────────────────────────────────────────────────────────────

export async function apiFetchLogs(limit?: number): Promise<RequestLog[]> {
  const path = limit ? `/admin/api/logs?limit=${limit}` : '/admin/api/logs';
  return apiFetch<RequestLog[]>(path);
}

export async function apiFetchRuntimeLogs(offset: number = 0, limit: number = 100): Promise<RuntimeLogsResponse> {
  return apiFetch<RuntimeLogsResponse>(`/admin/api/runtime-logs?offset=${offset}&limit=${limit}`);
}

// ── Status & Stats ────────────────────────────────────────────────────────

export async function apiFetchStatus(): Promise<AdminStatusResponse> {
  return apiFetch<AdminStatusResponse>('/admin/api/status');
}

export async function apiFetchStats(): Promise<StatsSnapshot> {
  return apiFetch<StatsSnapshot>('/admin/api/stats');
}

export async function apiFetchModels(): Promise<ModelListResponse> {
  return apiFetch<ModelListResponse>('/admin/api/models');
}

/** Comprehensive multilingual error localizer for all backend auth & system messages */
export function localizeAuthError(msg?: string, lang?: string): string {
  if (!msg) return '';
  const currentLang = lang || 'en';
  if (currentLang.startsWith('zh')) return msg;

  const isId = currentLang.startsWith('id');

  // Rate limit / lockout
  const lockoutMatch = msg.match(/(?:登录失败|请求)次数过多[，,\s]*请\s*(\d+)\s*秒后重试/);
  if (lockoutMatch) {
    const secs = lockoutMatch[1];
    return isId
      ? `Terlalu banyak percobaan gagal. Silakan coba lagi dalam ${secs} detik.`
      : `Too many failed login attempts. Please retry in ${secs} seconds.`;
  }

  if (msg.includes('密码错误')) {
    return isId ? 'Password salah. Silakan periksa kembali.' : 'Incorrect password. Please try again.';
  }
  if (msg.includes('未设置密码')) {
    return isId
      ? 'Password admin belum diatur. Silakan atur password terlebih dahulu.'
      : 'Admin password is not set. Please set up a password first.';
  }
  if (msg.includes('密码已设置')) {
    return isId
      ? 'Password admin sudah diatur. Silakan login.'
      : 'Admin password is already set. Please log in.';
  }
  if (msg.includes('密码长度至少') || msg.includes('不能少于6位') || msg.includes('至少 6 位')) {
    return isId
      ? 'Password harus memiliki panjang minimal 6 karakter.'
      : 'Password must be at least 6 characters.';
  }
  if (msg.includes('旧密码错误')) {
    return isId ? 'Password lama tidak sesuai.' : 'Incorrect old password.';
  }
  if (msg.includes('旧密码不能为空')) {
    return isId ? 'Password lama tidak boleh kosong.' : 'Old password cannot be empty.';
  }
  if (msg.includes('新密码不能为空')) {
    return isId ? 'Password baru tidak boleh kosong.' : 'New password cannot be empty.';
  }
  if (msg.includes('保存失败')) {
    return isId ? 'Gagal menyimpan konfigurasi.' : 'Failed to save configuration.';
  }
  if (msg.includes('请求格式错误')) {
    return isId ? 'Format data permintaan tidak valid.' : 'Invalid request data format.';
  }
  if (msg.includes('JWT 签发失败')) {
    return isId ? 'Gagal menerbitkan token sesi.' : 'Failed to issue session token.';
  }
  if (msg.includes('登录失败')) {
    return isId ? 'Gagal masuk. Periksa password Anda.' : 'Login failed. Please check your password.';
  }
  if (msg.includes('设置失败')) {
    return isId ? 'Gagal mengatur password.' : 'Setup failed.';
  }

  return msg;
}
