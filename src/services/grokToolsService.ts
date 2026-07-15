import { invoke } from '@tauri-apps/api/core';

export interface GrokRegistrationSettings {
  email_provider?: 'duckmail' | 'cloudflare' | 'cloudmail' | 'yyds';
  register_count?: number;
  enable_nsfw?: boolean;
  proxy?: string;
  browser_path?: string;
  duckmail_api_key?: string;
  cloudflare_api_base?: string;
  cloudflare_api_key?: string;
  cloudflare_auth_mode?: 'none' | 'bearer' | 'query';
  cloudmail_api_base?: string;
  cloudmail_public_token?: string;
  cloudmail_domains?: string;
  yyds_api_key?: string;
  yyds_jwt?: string;
  defaultDomains?: string;
  [key: string]: unknown;
}

export interface GrokToolsSettings {
  apiPort: number;
  apiAutoStart: boolean;
  registration: GrokRegistrationSettings;
}

export interface GrokToolsStatus {
  apiRunning: boolean;
  apiReady: boolean;
  registrationRunning: boolean;
  apiBaseUrl: string;
  apiKey?: string | null;
  settings: GrokToolsSettings;
}

export interface GrokToolsEvent {
  kind: string;
  level: 'info' | 'success' | 'error';
  message: string;
  data?: Record<string, unknown> | null;
}

export const getGrokToolsStatus = (): Promise<GrokToolsStatus> =>
  invoke('grok_tools_get_status');

export const updateGrokToolsSettings = (settings: GrokToolsSettings): Promise<GrokToolsStatus> =>
  invoke('grok_tools_update_settings', { settings });

export const startGrokApi = (): Promise<GrokToolsStatus> =>
  invoke('grok_tools_start_api');

export const stopGrokApi = (): Promise<GrokToolsStatus> =>
  invoke('grok_tools_stop_api');

export const startGrokRegistration = (): Promise<GrokToolsStatus> =>
  invoke('grok_tools_start_registration');

export const cancelGrokRegistration = (): Promise<GrokToolsStatus> =>
  invoke('grok_tools_cancel_registration');
