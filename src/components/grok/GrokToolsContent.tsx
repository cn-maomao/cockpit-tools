import { useCallback, useEffect, useMemo, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { CheckCircle2, Copy, Play, RefreshCw, Save, Server, Square, Workflow } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import {
  cancelGrokRegistration,
  getGrokToolsStatus,
  startGrokApi,
  startGrokRegistration,
  stopGrokApi,
  updateGrokToolsSettings,
  type GrokRegistrationSettings,
  type GrokToolsEvent,
  type GrokToolsSettings,
  type GrokToolsStatus,
} from '../../services/grokToolsService';

interface ProgressState {
  success: number;
  failed: number;
  pending: number;
  processed: number;
  total: number;
}

const EMPTY_PROGRESS: ProgressState = { success: 0, failed: 0, pending: 0, processed: 0, total: 0 };

export function GrokToolsContent() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<GrokToolsStatus | null>(null);
  const [settings, setSettings] = useState<GrokToolsSettings | null>(null);
  const [logs, setLogs] = useState<string[]>([]);
  const [progress, setProgress] = useState(EMPTY_PROGRESS);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [keyCopied, setKeyCopied] = useState(false);

  const refresh = useCallback(async () => {
    const next = await getGrokToolsStatus();
    setStatus(next);
    setSettings((current) => current ?? next.settings);
  }, []);

  useEffect(() => {
    void refresh().catch((reason) => setError(String(reason)));
    const unlisten = listen<GrokToolsEvent>('grok-tools:event', ({ payload }) => {
      if (payload.message) {
        setLogs((current) => [...current.slice(-199), payload.message]);
      }
      if (payload.kind === 'progress' && payload.data) {
        setProgress({
          success: Number(payload.data.success ?? 0),
          failed: Number(payload.data.failed ?? 0),
          pending: Number(payload.data.pending ?? 0),
          processed: Number(payload.data.processed ?? 0),
          total: Number(payload.data.total ?? 0),
        });
      }
      if (payload.kind === 'complete' || payload.kind === 'registration-exited' || payload.kind === 'account-imported' || payload.kind === 'api') {
        void refresh();
      }
      if (payload.level === 'error') setError(payload.message);
    });
    return () => { void unlisten.then((dispose) => dispose()); };
  }, [refresh]);

  const registration = settings?.registration ?? {};
  const provider = registration.email_provider ?? 'duckmail';
  const canStart = useMemo(() => {
    if (!settings || status?.registrationRunning) return false;
    if (provider === 'duckmail') return Boolean(registration.duckmail_api_key?.trim());
    if (provider === 'cloudflare') return Boolean(registration.cloudflare_api_base?.trim());
    if (provider === 'cloudmail') return Boolean(
      registration.cloudmail_api_base?.trim()
      && registration.cloudmail_admin_email?.trim()
      && registration.cloudmail_admin_password?.trim()
      && registration.cloudmail_domains?.trim()
    );
    return Boolean(registration.yyds_api_key?.trim() || registration.yyds_jwt?.trim());
  }, [provider, registration, settings, status?.registrationRunning]);

  const patchRegistration = (patch: Partial<GrokRegistrationSettings>) => {
    setSettings((current) => current ? {
      ...current,
      registration: { ...current.registration, ...patch },
    } : current);
  };

  const run = async (key: string, action: () => Promise<GrokToolsStatus>) => {
    setBusy(key);
    setError(null);
    try {
      const next = await action();
      setStatus(next);
      setSettings(next.settings);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(null);
    }
  };

  const save = async () => {
    if (!settings) return;
    await run('save', () => updateGrokToolsSettings(settings));
  };

  const startRegistration = async () => {
    if (!settings) return;
    setProgress(EMPTY_PROGRESS);
    setLogs([]);
    setBusy('register');
    setError(null);
    try {
      await updateGrokToolsSettings(settings);
      const next = await startGrokRegistration();
      setStatus(next);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(null);
    }
  };

  const copyApiUrl = async () => {
    if (!status) return;
    await navigator.clipboard.writeText(`${status.apiBaseUrl}/v1`);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1200);
  };

  const copyApiKey = async () => {
    if (!status?.apiKey) return;
    await navigator.clipboard.writeText(status.apiKey);
    setKeyCopied(true);
    window.setTimeout(() => setKeyCopied(false), 1200);
  };

  if (!settings || !status) {
    return <div className="grok-tools-loading"><RefreshCw className="spin" size={18} />{t('common.loading', '加载中...')}</div>;
  }

  return (
    <div className="grok-tools-content">
      <div className="grok-tools-intro">
        <Workflow size={20} />
        <div>
          <h3>{t('grok.tools.title', '自动注册与 Grok2API')}</h3>
          <p>{t('grok.tools.desc', '浏览器注册成功后，账号会立即导入内置 Grok2API，无需复制 Token 或运行外部命令。')}</p>
        </div>
      </div>

      {error && <div className="grok-tools-error">{error}</div>}

      <section className="grok-tool-card">
        <div className="grok-tool-card-header">
          <div><Server size={18} /><strong>Grok2API</strong></div>
          <span className={`grok-tool-status ${status.apiReady ? 'ready' : ''}`}>
            {status.apiReady ? t('common.running', '运行中') : t('common.stopped', '已停止')}
          </span>
        </div>
        <div className="grok-tool-grid compact">
          <label>
            <span>{t('grok.tools.apiPort', '服务端口')}</span>
            <input className="form-input" type="number" min={1} max={65535} value={settings.apiPort}
              disabled={status.apiRunning}
              onChange={(event) => setSettings({ ...settings, apiPort: Number(event.target.value) || 8000 })} />
          </label>
          <label>
            <span>{t('grok.tools.apiKey', 'API Key')}</span>
            <div className="grok-tool-input-action">
              <input className="form-input" type="password" readOnly value={status.apiKey ?? t('grok.tools.startToGenerate', '启动服务后自动生成')} />
              <button className="btn btn-secondary icon-only" disabled={!status.apiKey} onClick={() => void copyApiKey()} title={t('common.copy', '复制')}>
                {keyCopied ? <CheckCircle2 size={15} /> : <Copy size={15} />}
              </button>
            </div>
          </label>
          <label>
            <span>{t('grok.tools.baseUrl', 'OpenAI Base URL')}</span>
            <div className="grok-tool-input-action">
              <input className="form-input" readOnly value={`${status.apiBaseUrl}/v1`} />
              <button className="btn btn-secondary icon-only" onClick={() => void copyApiUrl()} title={t('common.copy', '复制')}>
                {copied ? <CheckCircle2 size={15} /> : <Copy size={15} />}
              </button>
            </div>
          </label>
        </div>
        <div className="grok-tool-actions">
          <label className="grok-tool-autostart">
            <input type="checkbox" checked={settings.apiAutoStart} onChange={(event) => setSettings({ ...settings, apiAutoStart: event.target.checked })} />
            <span>{t('grok.tools.autoStartApi', '随 Cockpit 自动启动')}</span>
          </label>
          <button className="btn btn-secondary" disabled={busy === 'save'} onClick={() => void save()}><Save size={14} />{t('common.save', '保存')}</button>
          {status.apiRunning ? (
            <button className="btn btn-secondary" disabled={busy === 'api' || status.registrationRunning} onClick={() => void run('api', stopGrokApi)}><Square size={14} />{t('common.stop', '停止')}</button>
          ) : (
            <button className="btn btn-primary" disabled={busy === 'api'} onClick={() => void run('api', startGrokApi)}><Play size={14} />{t('grok.tools.startApi', '启动 API 服务')}</button>
          )}
        </div>
      </section>

      <section className="grok-tool-card">
        <div className="grok-tool-card-header">
          <div><Workflow size={18} /><strong>{t('grok.tools.registration', '自动注册')}</strong></div>
          <span className={`grok-tool-status ${status.registrationRunning ? 'ready' : ''}`}>
            {status.registrationRunning ? t('common.running', '运行中') : t('common.ready', '就绪')}
          </span>
        </div>
        <div className="grok-tool-grid">
          <label>
            <span>{t('grok.tools.mailProvider', '邮箱服务')}</span>
            <select className="form-select" value={provider} onChange={(event) => patchRegistration({ email_provider: event.target.value as GrokRegistrationSettings['email_provider'] })}>
              <option value="duckmail">DuckMail</option><option value="cloudflare">Cloudflare Mail</option>
              <option value="cloudmail">CloudMail</option><option value="yyds">YYDS Mail</option>
            </select>
          </label>
          <label>
            <span>{t('grok.tools.count', '注册数量')}</span>
            <input className="form-input" type="number" min={1} max={100} value={Number(registration.register_count ?? 1)}
              onChange={(event) => patchRegistration({ register_count: Math.max(1, Number(event.target.value) || 1) })} />
          </label>
          {provider === 'duckmail' && <label className="wide"><span>DuckMail API Key</span><input className="form-input" type="password" value={registration.duckmail_api_key ?? ''} onChange={(event) => patchRegistration({ duckmail_api_key: event.target.value })} /></label>}
          {provider === 'cloudflare' && <>
            <label><span>Cloudflare API Base</span><input className="form-input" value={registration.cloudflare_api_base ?? ''} onChange={(event) => patchRegistration({ cloudflare_api_base: event.target.value })} /></label>
            <label><span>Cloudflare API Key</span><input className="form-input" type="password" value={registration.cloudflare_api_key ?? ''} onChange={(event) => patchRegistration({ cloudflare_api_key: event.target.value })} /></label>
          </>}
          {provider === 'cloudmail' && <>
            <label><span>CloudMail API Base</span><input className="form-input" value={registration.cloudmail_api_base ?? ''} onChange={(event) => patchRegistration({ cloudmail_api_base: event.target.value })} /></label>
            <label><span>CloudMail 管理员邮箱</span><input className="form-input" type="email" autoComplete="username" value={registration.cloudmail_admin_email ?? ''} onChange={(event) => patchRegistration({ cloudmail_admin_email: event.target.value })} /></label>
            <label><span>CloudMail 管理员密码</span><input className="form-input" type="password" autoComplete="current-password" value={registration.cloudmail_admin_password ?? ''} onChange={(event) => patchRegistration({ cloudmail_admin_password: event.target.value })} /></label>
            <label><span>{t('grok.tools.domains', '邮箱域名（逗号分隔）')}</span><input className="form-input" value={registration.cloudmail_domains ?? ''} onChange={(event) => patchRegistration({ cloudmail_domains: event.target.value })} /></label>
          </>}
          {provider === 'yyds' && <>
            <label><span>YYDS API Key</span><input className="form-input" type="password" value={registration.yyds_api_key ?? ''} onChange={(event) => patchRegistration({ yyds_api_key: event.target.value })} /></label>
            <label><span>YYDS JWT</span><input className="form-input" type="password" value={registration.yyds_jwt ?? ''} onChange={(event) => patchRegistration({ yyds_jwt: event.target.value })} /></label>
          </>}
          <label className="wide"><span>{t('grok.tools.proxy', '浏览器代理（可选）')}</span><input className="form-input" placeholder="http://127.0.0.1:7890" value={registration.proxy ?? ''} onChange={(event) => patchRegistration({ proxy: event.target.value })} /></label>
          <label className="wide"><span>{t('grok.tools.browserPath', 'Chrome / Edge 路径（留空自动检测）')}</span><input className="form-input" value={registration.browser_path ?? ''} onChange={(event) => patchRegistration({ browser_path: event.target.value })} /></label>
          <label className="grok-tool-check wide"><input type="checkbox" checked={Boolean(registration.enable_nsfw ?? true)} onChange={(event) => patchRegistration({ enable_nsfw: event.target.checked })} /><span>{t('grok.tools.enableNsfw', '注册后自动开启 NSFW 设置')}</span></label>
        </div>
        <div className="grok-tool-actions">
          <button className="btn btn-secondary" disabled={busy !== null || status.registrationRunning} onClick={() => void save()}><Save size={14} />{t('common.save', '保存')}</button>
          {status.registrationRunning ? (
            <button className="btn btn-danger" disabled={busy !== null} onClick={() => void run('cancel', cancelGrokRegistration)}><Square size={14} />{t('grok.tools.cancel', '停止注册')}</button>
          ) : (
            <button className="btn btn-primary" disabled={busy !== null || !canStart} onClick={() => void startRegistration()}><Play size={14} />{t('grok.tools.start', '开始全自动注册')}</button>
          )}
        </div>
      </section>

      {(status.registrationRunning || logs.length > 0) && <section className="grok-tool-card">
        <div className="grok-tool-progress-row"><strong>{t('grok.tools.progress', '任务进度')}</strong><span>{progress.processed}/{progress.total || Number(registration.register_count ?? 1)} · {t('grok.tools.successCount', '成功 {{count}}', { count: progress.success })} · {t('grok.tools.failedCount', '失败 {{count}}', { count: progress.failed })}</span></div>
        <div className="grok-tool-progress"><span style={{ width: `${progress.total ? Math.min(100, progress.processed / progress.total * 100) : 0}%` }} /></div>
        <div className="grok-tool-logs">{logs.map((line, index) => <div key={`${index}-${line}`}>{line}</div>)}</div>
      </section>}
    </div>
  );
}
