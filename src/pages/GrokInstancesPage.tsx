import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  Check,
  ChevronLeft,
  CircleCheck,
  CircleX,
  Copy,
  Play,
  RefreshCw,
  Terminal,
  X,
} from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { PlatformInstancesContent } from '../components/platform/PlatformInstancesContent';
import { DosageNotifyQuotaPreview } from '../components/platform/DosageNotifyQuotaPreview';
import { ModalErrorMessage } from '../components/ModalErrorMessage';
import { SingleSelectDropdown } from '../components/SingleSelectDropdown';
import { useEscClose } from '../hooks/useEscClose';
import { useLaunchTerminalOptions } from '../hooks/useLaunchTerminalOptions';
import { usePlatformRuntimeSupport } from '../hooks/usePlatformRuntimeSupport';
import * as grokInstanceService from '../services/grokInstanceService';
import type { GrokCliStatus } from '../services/grokInstanceService';
import { useGrokAccountStore } from '../stores/useGrokAccountStore';
import { useGrokInstanceStore } from '../stores/useGrokInstanceStore';
import {
  getGrokAccountDisplayEmail,
  getGrokPlanBadge,
  getGrokUsage,
  type GrokAccount,
} from '../types/grok';
import type { InstanceProfile } from '../types/instance';

interface GrokInstancesContentProps {
  accountsForSelect?: GrokAccount[];
}

interface GrokLaunchModalState {
  instanceId: string;
  instanceName: string;
  switchMessage: string;
  launchCommand: string;
  copied: boolean;
  executing: boolean;
  executeMessage: string | null;
  executeError: string | null;
  errorScrollKey: number;
}

const GROK_CLI_INSTALL_COMMAND =
  'curl -fsSL https://x.ai/cli/install.sh | bash';

export function GrokInstancesContent({
  accountsForSelect,
}: GrokInstancesContentProps = {}) {
  const { t, i18n } = useTranslation();
  const accountStore = useGrokAccountStore();
  const instanceStore = useGrokInstanceStore();
  const accounts = accountsForSelect ?? accountStore.accounts;
  const isSupported = usePlatformRuntimeSupport('desktop');
  const [launchModal, setLaunchModal] = useState<GrokLaunchModalState | null>(
    null,
  );
  const [cliStatus, setCliStatus] = useState<GrokCliStatus | null>(null);
  const [cliPath, setCliPath] = useState('');
  const [cliModalOpen, setCliModalOpen] = useState(false);
  const [cliStatusLoading, setCliStatusLoading] = useState(false);
  const [cliSaving, setCliSaving] = useState(false);
  const [cliError, setCliError] = useState<string | null>(null);
  const [cliErrorScrollKey, setCliErrorScrollKey] = useState(0);
  const [cliActionError, setCliActionError] = useState<string | null>(null);
  const [cliActionErrorScrollKey, setCliActionErrorScrollKey] = useState(0);
  const [installCommandCopied, setInstallCommandCopied] = useState(false);
  const [installExecuting, setInstallExecuting] = useState(false);
  const [installOpened, setInstallOpened] = useState(false);
  const [retryInstanceId, setRetryInstanceId] = useState<string | null>(null);
  const { terminalOptions, selectedTerminal, setSelectedTerminal } =
    useLaunchTerminalOptions();

  useEscClose(!!launchModal, () => setLaunchModal(null));
  useEscClose(cliModalOpen, () => {
    setCliModalOpen(false);
    setCliError(null);
    setCliActionError(null);
    setInstallCommandCopied(false);
    setInstallExecuting(false);
    setInstallOpened(false);
    setRetryInstanceId(null);
  });

  const applyCliStatus = useCallback((status: GrokCliStatus) => {
    setCliStatus(status);
    setCliPath(status.configuredPath || '');
  }, []);

  const reportCliError = useCallback((message: string) => {
    setCliError(message);
    setCliErrorScrollKey((current) => current + 1);
  }, []);

  const reportCliActionError = useCallback((message: string) => {
    setCliActionError(message);
    setCliActionErrorScrollKey((current) => current + 1);
  }, []);

  const resolveCliUnavailableMessage = useCallback(
    (status: GrokCliStatus) => {
      const configuredPath = status.configuredPath?.trim();
      return configuredPath
        ? t(
            'grok.instances.cliPathInvalid',
            '配置的 Grok CLI 路径无效：{{path}}',
            { path: configuredPath },
          )
        : t(
            'quickSettings.grok.cliMissing',
            '未检测到 Grok CLI，可填写自定义路径',
          );
    },
    [t],
  );

  const loadCliStatus = useCallback(
    async (showError: boolean) => {
      setCliStatusLoading(true);
      if (showError) {
        setCliError(null);
        setCliActionError(null);
      }
      try {
        const status = await grokInstanceService.getGrokCliStatus();
        applyCliStatus(status);
        return status;
      } catch (error) {
        if (showError) reportCliError(String(error));
        return null;
      } finally {
        setCliStatusLoading(false);
      }
    },
    [applyCliStatus, reportCliError],
  );

  useEffect(() => {
    void loadCliStatus(false);
  }, [loadCliStatus]);

  const accountMap = useMemo(() => {
    const map = new Map<string, GrokAccount>();
    accounts.forEach((account) => map.set(account.id, account));
    return map;
  }, [accounts]);

  const handleInstanceStarted = async (instance: InstanceProfile) => {
    const launchInfo = await grokInstanceService.getGrokInstanceLaunchCommand(
      instance.id,
    );
    const boundAccount = instance.bindAccountId
      ? accountMap.get(instance.bindAccountId)
      : undefined;
    const instanceName = instance.isDefault
      ? t('instances.defaultName', '默认实例')
      : instance.name || t('instances.defaultName', '默认实例');
    setLaunchModal({
      instanceId: instance.id,
      instanceName,
      switchMessage: boundAccount
        ? t('accounts.switched', '已切换至 {{email}}', {
            email: getGrokAccountDisplayEmail(boundAccount),
          })
        : t('instances.messages.launchPrepared', '启动命令已准备'),
      launchCommand: launchInfo.launchCommand,
      copied: false,
      executing: false,
      executeMessage: null,
      executeError: null,
      errorScrollKey: 0,
    });
  };

  const handleInstanceStartError = useCallback(
    async (_error: unknown, instance: InstanceProfile) => {
      let status: GrokCliStatus;
      try {
        status = await grokInstanceService.getGrokCliStatus();
      } catch {
        return false;
      }
      applyCliStatus(status);
      if (status.available) return false;

      setRetryInstanceId(instance.id);
      setCliModalOpen(true);
      reportCliError(resolveCliUnavailableMessage(status));
      return true;
    },
    [applyCliStatus, reportCliError, resolveCliUnavailableMessage],
  );

  const closeCliModal = () => {
    setCliModalOpen(false);
    setCliError(null);
    setCliActionError(null);
    setInstallCommandCopied(false);
    setInstallExecuting(false);
    setInstallOpened(false);
    setRetryInstanceId(null);
  };

  const openCliModal = () => {
    setRetryInstanceId(null);
    setCliError(null);
    setCliActionError(null);
    setInstallCommandCopied(false);
    setInstallExecuting(false);
    setInstallOpened(false);
    setCliModalOpen(true);
    void loadCliStatus(true);
  };

  const handleSaveCliPath = async () => {
    if (cliSaving || installExecuting) return;
    setCliSaving(true);
    setCliError(null);
    setCliActionError(null);
    try {
      const status = await grokInstanceService.updateGrokCliRuntimeConfig(
        cliPath,
      );
      applyCliStatus(status);
      if (!status.available) {
        reportCliError(resolveCliUnavailableMessage(status));
        return;
      }

      if (!retryInstanceId) return;
      try {
        const startedInstance = await instanceStore.startInstance(
          retryInstanceId,
        );
        await handleInstanceStarted(startedInstance);
        setRetryInstanceId(null);
        setCliModalOpen(false);
      } catch (error) {
        reportCliError(String(error));
      }
    } catch (error) {
      reportCliError(String(error));
    } finally {
      setCliSaving(false);
    }
  };

  const cliStatusText = cliStatus?.available
    ? t('quickSettings.grok.cliDetected', '已检测 {{version}} · {{path}}', {
        version: cliStatus.version || '--',
        path: cliStatus.binaryPath || '--',
      })
    : t(
        'quickSettings.grok.cliMissing',
        '未检测到 Grok CLI，可填写自定义路径',
      );

  const handleCopyInstallCommand = async () => {
    setCliActionError(null);
    setInstallCommandCopied(false);
    try {
      await navigator.clipboard.writeText(GROK_CLI_INSTALL_COMMAND);
      setInstallCommandCopied(true);
      window.setTimeout(() => setInstallCommandCopied(false), 1200);
    } catch {
      reportCliActionError(
        t('common.shared.export.copyFailed', '复制失败，请手动复制'),
      );
    }
  };

  const handleExecuteInstallCommand = async () => {
    if (installExecuting) return;
    setCliActionError(null);
    setInstallOpened(false);
    setInstallExecuting(true);
    try {
      await grokInstanceService.executeGrokCliInstallCommand(selectedTerminal);
      setInstallOpened(true);
    } catch (error) {
      reportCliActionError(String(error));
    } finally {
      setInstallExecuting(false);
    }
  };

  const handleCopyLaunchCommand = async () => {
    if (!launchModal) return;
    try {
      await navigator.clipboard.writeText(launchModal.launchCommand);
      setLaunchModal((current) =>
        current ? { ...current, copied: true, executeError: null } : current,
      );
      window.setTimeout(() => {
        setLaunchModal((current) =>
          current ? { ...current, copied: false } : current,
        );
      }, 1200);
    } catch {
      setLaunchModal((current) =>
        current
          ? {
              ...current,
              executeError: t(
                'common.shared.export.copyFailed',
                '复制失败，请手动复制',
              ),
              errorScrollKey: current.errorScrollKey + 1,
            }
          : current,
      );
    }
  };

  const handleExecuteInTerminal = async () => {
    if (!launchModal || launchModal.executing) return;
    setLaunchModal((current) =>
      current
        ? {
            ...current,
            executing: true,
            executeError: null,
            executeMessage: null,
          }
        : current,
    );
    try {
      const result = await grokInstanceService.executeGrokInstanceLaunchCommand(
        launchModal.instanceId,
        selectedTerminal,
      );
      setLaunchModal((current) =>
        current
          ? { ...current, executing: false, executeMessage: result }
          : current,
      );
    } catch (error) {
      setLaunchModal((current) =>
        current
          ? {
              ...current,
              executing: false,
              executeError: String(error),
              errorScrollKey: current.errorScrollKey + 1,
            }
          : current,
      );
    }
  };

  const handleTerminalChange = (terminal: string) => {
    setSelectedTerminal(terminal);
    setCliActionError(null);
    setInstallOpened(false);
    setLaunchModal((current) =>
      current
        ? { ...current, executeError: null, executeMessage: null }
        : current,
    );
  };

  return (
    <>
      <PlatformInstancesContent<GrokAccount>
        instanceStore={instanceStore}
        accounts={accounts}
        fetchAccounts={accountStore.fetchAccounts}
        renderAccountQuotaPreview={(account) => (
          <DosageNotifyQuotaPreview
            usage={getGrokUsage(account)}
            locale={i18n.language || 'zh-CN'}
            emptyText={t('instances.quota.empty', '暂无配额缓存')}
            normalText={t('grok.usageNormal', '正常')}
            abnormalText={t('grok.usageAbnormal', '异常')}
            abnormalDisplay="short"
          />
        )}
        renderAccountBadge={(account) => (
          <span className="instance-plan-badge">
            {getGrokPlanBadge(account)}
          </span>
        )}
        getAccountDisplayText={getGrokAccountDisplayEmail}
        getAccountSearchText={(account) =>
          [
            getGrokAccountDisplayEmail(account),
            account.first_name,
            account.last_name,
            account.principal_id,
            account.team_id,
            getGrokPlanBadge(account),
          ]
            .filter(Boolean)
            .join(' ')
        }
        appType="grok"
        isSupported={isSupported}
        unsupportedTitleKey="common.shared.instances.unsupported.title"
        unsupportedTitleDefault="暂不支持当前系统"
        unsupportedDescKey="grok.instances.unsupported"
        unsupportedDescDefault="Grok CLI 多开仅支持 macOS、Windows 和 Linux。"
        onInstanceStarted={handleInstanceStarted}
        onInstanceStartError={handleInstanceStartError}
        resolveStartSuccessMessage={() =>
          t('instances.messages.launchPrepared', '启动命令已准备')
        }
        toolbarExtraActions={
          <button
            type="button"
            className={`btn btn-secondary grok-cli-status-button${
              cliStatus?.available ? ' is-ready' : ' is-missing'
            }`}
            onClick={openCliModal}
            title={cliStatusText}
            aria-label={t('quickSettings.grok.title', 'Grok CLI 设置')}
          >
            <Terminal size={14} />
            <span>
              {cliStatusLoading
                ? t('common.loading', '加载中...')
                : cliStatus?.available
                  ? cliStatus.version ||
                    t('grok.instances.cliReady', '已检测')
                  : t('grok.instances.cliMissingShort', '未检测')}
            </span>
          </button>
        }
      />

      {cliModalOpen && (
        <div className="modal-overlay">
          <div
            className="modal grok-cli-settings-modal"
            onClick={(event) => event.stopPropagation()}
          >
            <div className="modal-header">
              <button
                className="btn btn-secondary icon-only"
                onClick={closeCliModal}
                title={t('common.back', '返回')}
                aria-label={t('common.back', '返回')}
              >
                <ChevronLeft size={14} />
              </button>
              <h2>{t('quickSettings.grok.title', 'Grok CLI 设置')}</h2>
              <button
                className="modal-close"
                onClick={closeCliModal}
                aria-label={t('common.close', '关闭')}
              >
                <X />
              </button>
            </div>
            <div className="modal-body">
              <div
                className={`grok-cli-runtime-status${
                  cliStatus?.available ? ' is-ready' : ' is-missing'
                }`}
              >
                {cliStatus?.available ? (
                  <CircleCheck size={18} />
                ) : (
                  <CircleX size={18} />
                )}
                <span>{cliStatusText}</span>
                <button
                  type="button"
                  className="btn btn-secondary icon-only"
                  onClick={() => void loadCliStatus(true)}
                  disabled={cliStatusLoading || cliSaving || installExecuting}
                  title={t('common.refresh', '刷新')}
                  aria-label={t('common.refresh', '刷新')}
                >
                  <RefreshCw
                    size={14}
                    className={cliStatusLoading ? 'icon-spin' : ''}
                  />
                </button>
              </div>
              {cliStatus && !cliStatus.available && (
                <div className="grok-cli-install-guide">
                  <strong>
                    {t(
                      'grok.instances.installCommand',
                      '官方安装命令',
                    )}
                  </strong>
                  <p>
                    {t(
                      'grok.instances.installHint',
                      '可在终端运行以下官方命令，安装完成后点击刷新。',
                    )}
                  </p>
                  <div className="grok-cli-install-command">
                    <code>{GROK_CLI_INSTALL_COMMAND}</code>
                    <button
                      type="button"
                      className="btn btn-secondary icon-only"
                      onClick={() => void handleCopyInstallCommand()}
                      title={
                        installCommandCopied
                          ? t('common.success', '成功')
                          : t('common.copy', '复制')
                      }
                      aria-label={t('common.copy', '复制')}
                    >
                      {installCommandCopied ? (
                        <Check size={14} />
                      ) : (
                        <Copy size={14} />
                      )}
                    </button>
                  </div>
                  <div className="grok-cli-install-actions">
                    <div className="grok-cli-install-terminal">
                      <label>
                        {t('instances.launchDialog.terminal', '终端')}
                      </label>
                      <SingleSelectDropdown
                        value={selectedTerminal}
                        onChange={handleTerminalChange}
                        options={terminalOptions}
                        disabled={installExecuting || cliSaving}
                      />
                    </div>
                    <button
                      type="button"
                      className="btn btn-primary"
                      onClick={() => void handleExecuteInstallCommand()}
                      disabled={installExecuting || cliSaving}
                    >
                      <Play size={14} />
                      {installExecuting
                        ? t('common.loading', '加载中...')
                        : t('grok.instances.runInTerminal', '终端执行')}
                    </button>
                  </div>
                  {installOpened && (
                    <div className="add-status success">
                      <Check size={14} />
                      <span>{t('common.success', '成功')}</span>
                    </div>
                  )}
                </div>
              )}
              <ModalErrorMessage
                message={cliActionError}
                scrollKey={cliActionErrorScrollKey}
              />
              <div className="form-group">
                <label>{t('quickSettings.grok.cliPath', 'CLI 路径')}</label>
                <input
                  className="form-input"
                  value={cliPath}
                  placeholder={cliStatus?.binaryPath || '~/.grok/bin/grok'}
                  onChange={(event) => {
                    setCliPath(event.target.value);
                    setCliError(null);
                  }}
                  disabled={cliSaving || installExecuting}
                  autoFocus
                />
                <ModalErrorMessage
                  message={cliError}
                  scrollKey={cliErrorScrollKey}
                />
              </div>
            </div>
            <div className="modal-footer">
              <button
                className="btn btn-secondary"
                onClick={closeCliModal}
                disabled={cliSaving || installExecuting}
              >
                {t('common.cancel', '取消')}
              </button>
              <button
                className="btn btn-primary"
                onClick={() => void handleSaveCliPath()}
                disabled={cliSaving || installExecuting}
              >
                {cliSaving
                  ? t('common.loading', '加载中...')
                  : retryInstanceId
                    ? t('grok.instances.saveAndRetry', '保存并重试')
                    : t('common.save', '保存')}
              </button>
            </div>
          </div>
        </div>
      )}

      {launchModal && (
        <div className="modal-overlay">
          <div
            className="modal modal-lg"
            onClick={(event) => event.stopPropagation()}
          >
            <div className="modal-header">
              <button
                className="btn btn-secondary icon-only"
                onClick={() => setLaunchModal(null)}
                title={t('common.back', '返回')}
                aria-label={t('common.back', '返回')}
              >
                <ChevronLeft size={14} />
              </button>
              <h2>{t('grok.instances.launchDialogTitle', '启动实例')}</h2>
              <button
                className="modal-close"
                onClick={() => setLaunchModal(null)}
                aria-label={t('common.close', '关闭')}
              >
                <X />
              </button>
            </div>
            <div className="modal-body">
              <div className="add-status success">
                <Check size={16} />
                <span>{launchModal.switchMessage}</span>
              </div>
              <ModalErrorMessage
                message={launchModal.executeError}
                scrollKey={launchModal.errorScrollKey}
              />
              <div className="form-group">
                <label>{t('instances.columns.instance', '实例')}</label>
                <input
                  className="form-input"
                  value={launchModal.instanceName}
                  readOnly
                />
              </div>
              <div className="form-group">
                <label>{t('instances.launchDialog.command', '启动命令')}</label>
                <textarea
                  className="form-input instance-args-input"
                  value={launchModal.launchCommand}
                  readOnly
                />
                <p className="form-hint">
                  {t(
                    'grok.instances.launchHint',
                    '可复制命令手动执行，或点击下方按钮直接在终端执行。',
                  )}
                </p>
              </div>
              <div className="form-group">
                <label>{t('instances.launchDialog.terminal', '终端')}</label>
                <SingleSelectDropdown
                  value={selectedTerminal}
                  onChange={handleTerminalChange}
                  options={terminalOptions}
                  disabled={launchModal.executing}
                  ariaLabel={t('instances.launchDialog.terminal', '终端')}
                />
              </div>
              {launchModal.executeMessage && (
                <div className="add-status success">
                  <Check size={16} />
                  <span>{launchModal.executeMessage}</span>
                </div>
              )}
            </div>
            <div className="modal-footer">
              <button
                className="btn btn-secondary"
                onClick={handleCopyLaunchCommand}
              >
                <Copy size={16} />
                {launchModal.copied
                  ? t('common.success', '成功')
                  : t('common.copy', '复制')}
              </button>
              <button
                className="btn btn-primary"
                onClick={handleExecuteInTerminal}
                disabled={launchModal.executing}
              >
                <Play size={16} />
                {launchModal.executing
                  ? t('common.loading', '加载中...')
                  : t('grok.instances.runInTerminal', '终端执行')}
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
