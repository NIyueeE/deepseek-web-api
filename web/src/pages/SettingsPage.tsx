import { useEffect, useState, useRef } from 'react';
import { useOutletContext } from 'react-router-dom';
import { apiFetchConfig, apiSaveConfig, localizeAuthError, type FullConfig } from '@/lib/api';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import {
  Shield,
  Server,
  Globe,
  Cpu,
  Eye,
  EyeOff,
  Save,
  CheckCircle2,
  AlertCircle,
  Lock,
  Search,
} from 'lucide-react';
import { Skeleton } from '@/components/ui/skeleton';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';

export function SettingsPage() {
  const { t, i18n } = useTranslation();
  const [config, setConfig] = useState<FullConfig | null>(null);
  const [initialConfig, setInitialConfig] = useState<string>('');
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<{ type: 'ok' | 'err'; text: string } | null>(null);

  // Password change state
  const [oldPassword, setOldPassword] = useState('');
  const [newPassword, setNewPassword] = useState('');
  const [confirmPassword, setConfirmPassword] = useState('');
  const [showOldPass, setShowOldPass] = useState(false);
  const [showNewPass, setShowNewPass] = useState(false);

  useEffect(() => {
    apiFetchConfig()
      .then((cfg) => {
        setConfig(cfg);
        setInitialConfig(JSON.stringify(cfg));
      })
      .catch(() => setMessage({ type: 'err', text: t('config.loadFailed') }));
  }, [t]);

  const { isSidebarCollapsed } = useOutletContext<{ isSidebarCollapsed?: boolean }>() || {};
  const [isHeaderActionVisible, setIsHeaderActionVisible] = useState(true);
  const headerActionRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = headerActionRef.current;
    if (!el) return;
    const observer = new IntersectionObserver(
      ([entry]) => {
        setIsHeaderActionVisible(entry.isIntersecting);
      },
      { threshold: 0.1 }
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, [config]);

  const hasPasswordChanges = Boolean(newPassword || oldPassword || confirmPassword);
  const isDirty = config ? JSON.stringify(config) !== initialConfig || hasPasswordChanges : false;

  if (!config) {
    return (
      <div className="space-y-6 max-w-5xl">
        <div className="flex items-center justify-between pb-3 border-b">
          <div className="space-y-1.5">
            <Skeleton className="h-7 w-48" />
            <Skeleton className="h-4 w-72" />
          </div>
        </div>
        <div className="space-y-5">
          {Array.from({ length: 4 }).map((_, idx) => (
            <Card key={idx} className="p-4 space-y-4 border shadow-sm">
              <div className="flex items-center justify-between">
                <Skeleton className="h-5 w-40" />
                <Skeleton className="h-5 w-20 rounded-md" />
              </div>
              <Skeleton className="h-16 w-full rounded-xl" />
            </Card>
          ))}
        </div>
      </div>
    );
  }

  const update = <T,>(path: string[], value: T) => {
    setConfig((prev) => {
      if (!prev) return prev;
      const next = structuredClone(prev) as unknown as Record<string, unknown>;
      let obj: Record<string, unknown> = next;
      for (let i = 0; i < path.length - 1; i++) {
        obj = obj[path[i]] as Record<string, unknown>;
      }
      obj[path[path.length - 1]] = value as unknown;
      return next as unknown as FullConfig;
    });
  };

  const handleSave = async () => {
    if (newPassword && newPassword !== confirmPassword) {
      setMessage({ type: 'err', text: t('settings.passwordMismatch') });
      return;
    }
    if (newPassword && newPassword.length < 6) {
      setMessage({ type: 'err', text: t('settings.passwordMinLength') });
      return;
    }

    setSaving(true);
    setMessage(null);
    try {
      const body: Record<string, unknown> = {
        server: config.server,
        ds_core: config.ds_core,
        deepseek: config.ds_core,
        accounts: config.ds_core.accounts,
        proxy: config.proxy,
        admin: {
          password_hash: '',
          jwt_secret: '',
          jwt_issued_at: config.admin.jwt_issued_at,
          old_password: oldPassword,
          new_password: newPassword,
        },
        api_keys: config.api_keys.map((k) => ({
          key: k.key,
          description: k.description,
        })),
      };
      const res = await apiSaveConfig(body);
      if (res.ok) {
        setMessage({ type: 'ok', text: t('config.saveSuccess') });
        setOldPassword('');
        setNewPassword('');
        setConfirmPassword('');
        const fresh = await apiFetchConfig();
        setConfig(fresh);
        setInitialConfig(JSON.stringify(fresh));
      }
    } catch (e: unknown) {
      const rawMsg = e instanceof Error ? e.message : String(e);
      setMessage({ type: 'err', text: localizeAuthError(rawMsg, i18n.language) || t('config.saveFailed') });
    } finally {
      setSaving(false);
    }
  };

  const handleCancel = () => {
    if (confirm(t('config.cancelConfirm'))) {
      setOldPassword('');
      setNewPassword('');
      setConfirmPassword('');
      apiFetchConfig()
        .then((cfg) => {
          setConfig(cfg);
          setInitialConfig(JSON.stringify(cfg));
        })
        .catch(() => setMessage({ type: 'err', text: t('config.loadFailed') }));
    }
  };

  return (
    <div className="space-y-6 max-w-5xl pb-24 sm:pb-28 relative">
      {/* Page Header */}
      <div className="flex items-center justify-between gap-3 pb-3 border-b min-w-0">
        <div className="min-w-0 flex-1">
          <h1 className="text-xl sm:text-2xl font-bold tracking-tight flex items-center gap-2.5 truncate">
            <Shield className="h-5 w-5 sm:h-6 sm:w-6 text-primary shrink-0" />
            <span className="truncate">{t('settings.title')}</span>
          </h1>
          <p className="text-xs sm:text-sm text-muted-foreground mt-0.5 truncate">
            {t('settings.subtitle')}
          </p>
        </div>

        <div ref={headerActionRef} className="flex items-center gap-2 shrink-0">
          <Button
            variant="outline"
            size="sm"
            onClick={handleCancel}
            disabled={saving || !isDirty}
            className="h-8 text-xs shrink-0"
          >
            {t('config.cancel')}
          </Button>
          <Button
            size="sm"
            onClick={handleSave}
            disabled={saving}
            className="h-8 gap-1.5 text-xs shadow-sm shrink-0"
          >
            <Save className="h-3.5 w-3.5" />
            <span>{saving ? t('config.saving') : t('config.save')}</span>
          </Button>
        </div>
      </div>

      {/* Alert Banner */}
      {message && (
        <div
          className={`flex items-center gap-2.5 p-3.5 rounded-xl text-xs font-medium border ${
            message.type === 'err'
              ? 'bg-destructive/10 text-destructive border-destructive/20'
              : 'bg-emerald-500/10 text-emerald-600 dark:text-emerald-400 border-emerald-500/20'
          }`}
        >
          {message.type === 'err' ? (
            <AlertCircle className="h-4 w-4 shrink-0" />
          ) : (
            <CheckCircle2 className="h-4 w-4 shrink-0" />
          )}
          <span>{message.text}</span>
        </div>
      )}

      {/* ── 1. Admin Security & Credentials ───────────────────────── */}
      <Card className="border shadow-sm">
        <CardHeader className="pb-3">
          <div className="flex items-center justify-between">
            <CardTitle className="text-base font-semibold flex items-center gap-2">
              <Lock className="h-4 w-4 text-primary" />
              <span>{t('settings.securityTitle')}</span>
            </CardTitle>
            <Badge variant="outline" className="text-emerald-600 dark:text-emerald-400 bg-emerald-500/10 border-emerald-500/30 text-xs">
              {config.admin.password_set ? t('config.admin.passwordSet') : t('config.admin.passwordNotSet')}
            </Badge>
          </div>
          <CardDescription className="text-xs">
            {t('settings.securityDescription')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4 pt-2">
          <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
            <div>
              <label htmlFor="adm-old-pass" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.admin.oldPassword')}
              </label>
              <div className="relative">
                <Input
                  id="adm-old-pass"
                  type={showOldPass ? 'text' : 'password'}
                  placeholder={t('config.admin.oldPasswordPlaceholder')}
                  value={oldPassword}
                  onChange={(e) => setOldPassword(e.target.value)}
                  className="pr-9 text-xs"
                />
                <button
                  type="button"
                  onClick={() => setShowOldPass(!showOldPass)}
                  className="absolute right-2.5 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                  aria-label="Toggle old password visibility"
                >
                  {showOldPass ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                </button>
              </div>
            </div>

            <div>
              <label htmlFor="adm-new-pass" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.admin.newPassword')}
              </label>
              <div className="relative">
                <Input
                  id="adm-new-pass"
                  type={showNewPass ? 'text' : 'password'}
                  placeholder={t('config.admin.newPasswordPlaceholder')}
                  value={newPassword}
                  onChange={(e) => setNewPassword(e.target.value)}
                  className="pr-9 text-xs"
                />
                <button
                  type="button"
                  onClick={() => setShowNewPass(!showNewPass)}
                  className="absolute right-2.5 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                  aria-label="Toggle new password visibility"
                >
                  {showNewPass ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                </button>
              </div>
            </div>

            <div>
              <label htmlFor="adm-confirm-pass" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('settings.confirmPassword')}
              </label>
              <Input
                id="adm-confirm-pass"
                type="password"
                placeholder={t('settings.confirmPasswordPlaceholder')}
                value={confirmPassword}
                onChange={(e) => setConfirmPassword(e.target.value)}
                className="text-xs"
              />
            </div>
          </div>
        </CardContent>
      </Card>

      {/* ── 2. Server & Network ───────────────────────────────────── */}
      <Card className="border shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="text-base font-semibold flex items-center gap-2">
            <Server className="h-4 w-4 text-primary" />
            <span>{t('config.sections.server')}</span>
          </CardTitle>
          <CardDescription className="text-xs">
            {t('settings.serverDescription')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4 pt-2">
          <div className="grid grid-cols-1 sm:grid-cols-3 gap-4">
            <div>
              <label htmlFor="set-srv-host" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.server.host')}
              </label>
              <Input
                id="set-srv-host"
                value={config.server.host}
                onChange={(e) => update(['server', 'host'], e.target.value)}
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-srv-port" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.server.port')}
              </label>
              <Input
                id="set-srv-port"
                type="number"
                value={config.server.port}
                onChange={(e) => update(['server', 'port'], Number(e.target.value))}
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-srv-cors" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.server.corsOrigins')}
              </label>
              <Input
                id="set-srv-cors"
                value={config.server.cors_origins.join(', ')}
                onChange={(e) =>
                  update(
                    ['server', 'cors_origins'],
                    e.target.value.split(/,\s*/).filter(Boolean)
                  )
                }
                className="font-mono text-xs"
              />
            </div>
          </div>
        </CardContent>
      </Card>

      {/* ── 3. Proxy Settings ─────────────────────────────────────── */}
      <Card className="border shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="text-base font-semibold flex items-center gap-2">
            <Globe className="h-4 w-4 text-primary" />
            <span>{t('config.sections.proxy')}</span>
          </CardTitle>
          <CardDescription className="text-xs">
            {t('settings.proxyDescription')}
          </CardDescription>
        </CardHeader>
        <CardContent className="pt-2">
          <div>
            <label htmlFor="set-prx-url" className="text-xs font-medium text-muted-foreground block mb-1.5">
              {t('config.proxy.url')}
            </label>
            <Input
              id="set-prx-url"
              value={config.proxy?.url || ''}
              placeholder={t('config.proxy.placeholder')}
              onChange={(e) => update(['proxy', 'url'], e.target.value || null)}
              className="font-mono text-xs"
            />
          </div>
        </CardContent>
      </Card>

      {/* ── 4. DeepSeek Engine & Client Parameters ────────────────── */}
      <Card className="border shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="text-base font-semibold flex items-center gap-2">
            <Cpu className="h-4 w-4 text-primary" />
            <span>{t('settings.engineTitle')}</span>
          </CardTitle>
          <CardDescription className="text-xs">
            {t('settings.engineDescription')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4 pt-2">
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            <div>
              <label htmlFor="set-ds-api" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.deepseek.apiBase')}
              </label>
              <Input
                id="set-ds-api"
                value={config.ds_core.api_base}
                onChange={(e) => update(['ds_core', 'api_base'], e.target.value)}
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-ds-wasm" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.deepseek.wasmUrl')}
              </label>
              <Input
                id="set-ds-wasm"
                value={config.ds_core.wasm_url}
                onChange={(e) => update(['ds_core', 'wasm_url'], e.target.value)}
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-ds-ua" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.deepseek.userAgent')}
              </label>
              <Input
                id="set-ds-ua"
                value={config.ds_core.user_agent}
                onChange={(e) => update(['ds_core', 'user_agent'], e.target.value)}
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-ds-ver" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.deepseek.clientVersion')}
              </label>
              <Input
                id="set-ds-ver"
                value={config.ds_core.client_version}
                onChange={(e) => update(['ds_core', 'client_version'], e.target.value)}
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-ds-plat" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.deepseek.clientPlatform')}
              </label>
              <Input
                id="set-ds-plat"
                value={config.ds_core.client_platform}
                onChange={(e) => update(['ds_core', 'client_platform'], e.target.value)}
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-ds-loc" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.deepseek.clientLocale')}
              </label>
              <Input
                id="set-ds-loc"
                value={config.ds_core.client_locale}
                onChange={(e) => update(['ds_core', 'client_locale'], e.target.value)}
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-ds-os" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.deepseek.clientOs')}
              </label>
              <Input
                id="set-ds-os"
                value={config.ds_core.client_os}
                onChange={(e) => update(['ds_core', 'client_os'], e.target.value)}
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-ds-bundle" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.deepseek.clientBundleId')}
              </label>
              <Input
                id="set-ds-bundle"
                value={config.ds_core.client_bundle_id}
                onChange={(e) => update(['ds_core', 'client_bundle_id'], e.target.value)}
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-ds-devid" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.deepseek.clientDeviceId')}
              </label>
              <Input
                id="set-ds-devid"
                value={config.ds_core.client_device_id}
                onChange={(e) => update(['ds_core', 'client_device_id'], e.target.value)}
                className="font-mono text-xs"
                placeholder={t('config.deepseek.clientDeviceIdPlaceholder')}
              />
            </div>

            <div>
              <label htmlFor="set-ds-devmodel" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.deepseek.clientDeviceModel')}
              </label>
              <Input
                id="set-ds-devmodel"
                value={config.ds_core.client_device_model}
                onChange={(e) => update(['ds_core', 'client_device_model'], e.target.value)}
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-ds-tz" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('config.deepseek.clientTimezoneOffset')}
              </label>
              <Input
                id="set-ds-tz"
                value={config.ds_core.client_timezone_offset}
                onChange={(e) => update(['ds_core', 'client_timezone_offset'], e.target.value)}
                className="font-mono text-xs"
              />
            </div>
          </div>
        </CardContent>
      </Card>

      {/* ── 5. Search Mode Default ────────────────────────────────── */}
      <Card className="border shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="text-base font-semibold flex items-center gap-2">
            <Search className="h-4 w-4 text-primary" />
            <span>{t('settings.quota')}</span>
          </CardTitle>
          <CardDescription className="text-xs">
            {t('settings.searchModeDesc')}
          </CardDescription>
        </CardHeader>
        <CardContent className="pt-2 space-y-4">
          <div>
            <label htmlFor="set-quota" className="text-xs font-medium text-muted-foreground block mb-1.5">
              {t('settings.quotaLabel')}
            </label>
            <Input
              id="set-quota"
              type="number"
              value={config.ds_core.hourly_request_quota}
              onChange={(e) =>
                update(['ds_core', 'hourly_request_quota'], Number(e.target.value))
              }
              className="font-mono text-xs"
            />
            <p className="text-[11px] text-muted-foreground mt-1.5">{t('settings.quotaDesc')}</p>
          </div>
          <label
            htmlFor="set-default-search"
            className="flex items-center gap-3 cursor-pointer select-none"
          >
            <input
              id="set-default-search"
              type="checkbox"
              checked={config.ds_core.default_search_enabled}
              onChange={(e) =>
                update(['ds_core', 'default_search_enabled'], e.target.checked)
              }
              className="h-4 w-4 rounded border-input accent-primary cursor-pointer"
            />
            <span className="text-xs font-mono">
              default_search_enabled = {String(config.ds_core.default_search_enabled)}
            </span>
          </label>
          <div>
            <label
              htmlFor="set-hif-enabled"
              className="flex items-center gap-3 cursor-pointer select-none"
            >
              <input
                id="set-hif-enabled"
                type="checkbox"
                checked={config.ds_core.hif_enabled}
                onChange={(e) => update(['ds_core', 'hif_enabled'], e.target.checked)}
                className="h-4 w-4 rounded border-input accent-primary cursor-pointer"
              />
              <span className="text-xs font-mono">
                hif_enabled = {String(config.ds_core.hif_enabled)}
              </span>
            </label>
            <p className="text-[11px] text-muted-foreground mt-1.5">{t('settings.hifDesc')}</p>
          </div>
        </CardContent>
      </Card>

      {/* ── 6. Responses API Context Cache ────────────────────────── */}
      <Card className="border shadow-sm">
        <CardHeader className="pb-3">
          <CardTitle className="text-base font-semibold flex items-center gap-2">
            <Globe className="h-4 w-4 text-primary" />
            <span>{t('settings.responsesStore')}</span>
          </CardTitle>
          <CardDescription className="text-xs">
            {t('settings.responsesStoreDesc')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4 pt-2">
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            <div>
              <label htmlFor="set-store-cap" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('settings.storeCapacity')}
              </label>
              <Input
                id="set-store-cap"
                type="number"
                value={config.ds_core.responses_store_capacity}
                onChange={(e) =>
                  update(['ds_core', 'responses_store_capacity'], Number(e.target.value))
                }
                className="font-mono text-xs"
              />
            </div>

            <div>
              <label htmlFor="set-store-ttl" className="text-xs font-medium text-muted-foreground block mb-1.5">
                {t('settings.storeTtl')}
              </label>
              <Input
                id="set-store-ttl"
                type="number"
                value={config.ds_core.responses_store_ttl_secs}
                onChange={(e) =>
                  update(['ds_core', 'responses_store_ttl_secs'], Number(e.target.value))
                }
                className="font-mono text-xs"
              />
            </div>
          </div>
        </CardContent>
      </Card>

      {/* ── Floating Action Bar (Docked to Bottom of Viewport, Appears Only When Header Actions Are Scrolled Out of View) ── */}
      <div
        className={cn(
          'fixed bottom-20 md:bottom-4 left-3.5 right-3.5 md:left-20 md:right-6 z-40 max-w-5xl mx-auto flex items-center justify-between p-3 sm:p-3.5 rounded-2xl bg-card/95 backdrop-blur-md border shadow-xl ring-1 ring-black/5 dark:ring-white/10 transition-all duration-300',
          isSidebarCollapsed ? 'lg:left-20 lg:right-6' : 'lg:left-72 lg:right-8',
          isHeaderActionVisible
            ? 'opacity-0 translate-y-6 pointer-events-none'
            : 'opacity-100 translate-y-0 pointer-events-auto'
        )}
      >
        <div className="flex items-center gap-2.5 min-w-0">
          <span className="flex h-2.5 w-2.5 relative shrink-0">
            {isDirty ? (
              <>
                <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-amber-400 opacity-75" />
                <span className="relative inline-flex rounded-full h-2.5 w-2.5 bg-amber-500" />
              </>
            ) : (
              <span className="relative inline-flex rounded-full h-2.5 w-2.5 bg-green-500" />
            )}
          </span>
          <span className="text-xs font-medium text-foreground truncate">
            {isDirty
              ? t('settings.unsavedChanges')
              : t('settings.allSynced')}
          </span>
        </div>

        <div className="flex items-center gap-2 shrink-0">
          <Button
            variant="outline"
            size="sm"
            onClick={handleCancel}
            disabled={saving || !isDirty}
            className="h-8 text-xs shrink-0"
          >
            {t('config.cancel')}
          </Button>
          <Button
            size="sm"
            onClick={handleSave}
            disabled={saving || !isDirty}
            className="h-8 gap-1.5 text-xs shadow-sm shrink-0"
          >
            <Save className="h-3.5 w-3.5" />
            <span>{saving ? t('config.saving') : t('config.save')}</span>
          </Button>
        </div>
      </div>
    </div>
  );
}
