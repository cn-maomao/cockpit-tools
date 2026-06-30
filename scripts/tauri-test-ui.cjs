#!/usr/bin/env node

const { spawn } = require('node:child_process');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..');
const UI_DEV_SERVER_INFO_PATH = path.join(ROOT, '.tmp', 'platform-ui-dev', 'server.json');
const PACKAGE_DEV_SERVER_INFO_PATH = path.join(ROOT, '.tmp', 'platform-dev', 'server.json');
const TEST_CONFIG_PATH = 'src-tauri/tauri.test.conf.json';
const PLATFORM_PACKAGE_TEST_INDEX_URL =
  'https://raw.githubusercontent.com/jlcodes99/cockpit-tools/platform-test/platform-packages/index.test.json';
const CODEX_API_SERVICE_PORT = process.env.COCKPIT_CODEX_API_SERVICE_PORT || '12345';
const DEFAULT_UI_HOST = '127.0.0.1';
const DEFAULT_UI_PORT = 14522;
const DEFAULT_PACKAGE_PORT = 14520;
const DEFAULT_START_TIMEOUT_MS = 10 * 60 * 1000;

function fail(message) {
  console.error(`[tauri-test-ui] ${message}`);
  process.exit(1);
}

function usage() {
  console.log(`Usage:
  npm run tauri:test:ui -- [options]

Options:
  --platform <id[,id...]>  Serve selected platform UI(s). Defaults to all platform packages.
  --ui-port <port>         Local platform UI dev server port. Defaults to ${DEFAULT_UI_PORT}.
  --ui-host <host>         Local platform UI dev server host. Defaults to ${DEFAULT_UI_HOST}.
  --package-port <port>    Local platform package dev server port. Defaults to ${DEFAULT_PACKAGE_PORT}.
  --index-url <url>        Override platform package index URL used by the Test app.
  --dev-host               Launch Test app through tauri dev. Default.
  --release-host           Build/launch the local Test release executable instead of tauri dev.
  --no-build-app           Launch the existing local Test release build; implies --release-host.
  --no-package-reload      Do not start the local package reload server.
  --no-watch               Build platform UI once and keep serving without source watching.

Examples:
  npm run tauri:test:ui
  npm run tauri:test:ui -- --platform codex
  npm run tauri:test:ui -- --platform zed --release-host
  npm run tauri:test:ui -- --platform codex --no-build-app
  npm run tauri:test:ui -- --platform codex,zed --ui-port 14524
`);
}

function parseArgs(argv) {
  const args = {
    platforms: [],
    uiHost: DEFAULT_UI_HOST,
    uiPort: DEFAULT_UI_PORT,
    packagePort: DEFAULT_PACKAGE_PORT,
    buildApp: true,
    hostMode: 'dev',
    hostModeExplicit: false,
    packageReload: true,
    watch: true,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--help' || arg === '-h') {
      usage();
      process.exit(0);
    }
    if (arg === '--no-build-app') {
      args.buildApp = false;
      if (!args.hostModeExplicit) {
        args.hostMode = 'release';
      }
      continue;
    }
    if (arg === '--dev-host') {
      args.hostMode = 'dev';
      args.hostModeExplicit = true;
      continue;
    }
    if (arg === '--release-host') {
      args.hostMode = 'release';
      args.hostModeExplicit = true;
      continue;
    }
    if (arg === '--no-package-reload') {
      args.packageReload = false;
      continue;
    }
    if (arg === '--no-watch') {
      args.watch = false;
      continue;
    }

    const next = argv[index + 1];
    if (!next || next.startsWith('--')) fail(`Missing value for ${arg}`);
    index += 1;

    if (arg === '--platform') {
      args.platforms.push(
        ...next
          .split(',')
          .map((value) => value.trim())
          .filter(Boolean),
      );
    } else if (arg === '--ui-port') {
      const port = Number.parseInt(next, 10);
      if (!Number.isInteger(port) || port <= 0 || port > 65535) fail(`Invalid --ui-port: ${next}`);
      args.uiPort = port;
    } else if (arg === '--ui-host') {
      args.uiHost = next;
    } else if (arg === '--package-port') {
      const port = Number.parseInt(next, 10);
      if (!Number.isInteger(port) || port <= 0 || port > 65535) fail(`Invalid --package-port: ${next}`);
      args.packagePort = port;
    } else if (arg === '--index-url') {
      args.indexUrl = next;
    } else {
      fail(`Unknown argument: ${arg}`);
    }
  }

  args.platforms = Array.from(new Set(args.platforms));
  return args;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function readServerInfo(filePath) {
  if (!fs.existsSync(filePath)) return null;
  try {
    return JSON.parse(fs.readFileSync(filePath, 'utf8'));
  } catch {
    return null;
  }
}

function getJson(url) {
  return new Promise((resolve) => {
    const request = http.get(url, { timeout: 2000 }, (response) => {
      response.resume();
      response.on('end', () => resolve(response.statusCode && response.statusCode >= 200 && response.statusCode < 300));
    });
    request.on('timeout', () => {
      request.destroy();
      resolve(false);
    });
    request.on('error', () => resolve(false));
  });
}

async function waitForServer(child, infoPath, urlFromInfo, label) {
  const startedAt = Date.now();
  while (Date.now() - startedAt < DEFAULT_START_TIMEOUT_MS) {
    if (child.exitCode !== null) {
      fail(`${label} exited early with code ${child.exitCode}`);
    }

    const info = readServerInfo(infoPath);
    const url = info ? urlFromInfo(info) : null;
    if (url && await getJson(`${url}/health`)) {
      return info;
    }
    await sleep(500);
  }
  fail(`timed out waiting for ${label}`);
}

function spawnNode(scriptPath, args, env = {}) {
  return spawn(process.execPath, [scriptPath, ...args], {
    cwd: ROOT,
    env: { ...process.env, ...env },
    stdio: 'inherit',
    shell: false,
  });
}

function startPlatformUiDev(args) {
  fs.rmSync(UI_DEV_SERVER_INFO_PATH, { force: true });
  const commandArgs = [
    '--host',
    args.uiHost,
    '--port',
    String(args.uiPort),
  ];
  if (args.platforms.length > 0) {
    commandArgs.push('--platform', args.platforms.join(','));
  }
  if (!args.watch) {
    commandArgs.push('--no-watch');
  }
  console.log('[tauri-test-ui] starting platform UI dev server...');
  return spawnNode('scripts/platform-ui-dev.cjs', commandArgs);
}

function startPackageDev(args) {
  fs.rmSync(PACKAGE_DEV_SERVER_INFO_PATH, { force: true });
  const commandArgs = [
    '--host',
    args.uiHost,
    '--port',
    String(args.packagePort),
    '--lazy',
    '--no-build-ui',
    '--build-adapters',
  ];
  if (args.platforms.length > 0) {
    commandArgs.push('--platform', args.platforms.join(','));
  }
  console.log('[tauri-test-ui] starting platform package dev server...');
  return spawnNode('scripts/platform-dev-serve.cjs', commandArgs);
}

function startTestReleaseHost(args, uiDevBaseUrl, packageInfo) {
  const commandArgs = ['--ui-dev', '--ui-dev-url', uiDevBaseUrl];
  if (!args.buildApp) {
    commandArgs.push('--no-build');
  }
  const indexUrl = args.indexUrl || packageInfo?.indexUrl;
  if (indexUrl) {
    commandArgs.push('--index-url', indexUrl);
  }
  console.log('[tauri-test-ui] starting Test desktop app...');
  return spawnNode('scripts/tauri-test-local.cjs', commandArgs, packageInfo?.reloadUrl ? {
    COCKPIT_PLATFORM_PACKAGE_DEV_RELOAD_URL: packageInfo.reloadUrl,
  } : {});
}

function buildDevHostEnv(args, uiDevBaseUrl, packageInfo) {
  const indexUrl = args.indexUrl || packageInfo?.indexUrl || PLATFORM_PACKAGE_TEST_INDEX_URL;
  return {
    COCKPIT_TOOLS_PROFILE: 'test',
    COCKPIT_CODEX_API_SERVICE_PORT: CODEX_API_SERVICE_PORT,
    COCKPIT_TOOLS_API_PORT: CODEX_API_SERVICE_PORT,
    VITE_COCKPIT_TOOLS_PROFILE: 'test',
    COCKPIT_PLATFORM_PACKAGE_INDEX_URL: indexUrl,
    COCKPIT_PLATFORM_PACKAGE_PREFER_LOCAL_SOURCE: '0',
    COCKPIT_PLATFORM_PACKAGE_STRICT_LOCAL_SOURCE: '0',
    COCKPIT_PLATFORM_PACKAGE_BOOTSTRAP: '0',
    COCKPIT_PLATFORM_PACKAGE_WORKSPACE_INDEX: '0',
    COCKPIT_PLATFORM_UI_DEV_BASE_URL: uiDevBaseUrl,
    ...(packageInfo?.reloadUrl ? {
      COCKPIT_PLATFORM_PACKAGE_DEV_RELOAD_URL: packageInfo.reloadUrl,
    } : {}),
  };
}

function startTestDevHost(args, uiDevBaseUrl, packageInfo) {
  const commandArgs = ['dev', '--config', TEST_CONFIG_PATH];
  console.log('[tauri-test-ui] starting Test desktop app with tauri dev host...');
  return spawnNode('scripts/tauri.cjs', commandArgs, buildDevHostEnv(args, uiDevBaseUrl, packageInfo));
}

function startTestApp(args, uiDevBaseUrl, packageInfo) {
  if (args.hostMode === 'release') {
    return startTestReleaseHost(args, uiDevBaseUrl, packageInfo);
  }
  return startTestDevHost(args, uiDevBaseUrl, packageInfo);
}

function terminate(child, signal) {
  if (!child || child.exitCode !== null || child.killed) return;
  child.kill(signal);
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const uiDevChild = startPlatformUiDev(args);
  const packageDevChild = args.packageReload ? startPackageDev(args) : null;
  let testAppChild = null;
  let shuttingDown = false;

  const shutdown = (signal, exitCode) => {
    if (shuttingDown) return;
    shuttingDown = true;
    terminate(testAppChild, signal);
    terminate(packageDevChild, signal);
    terminate(uiDevChild, signal);
    setTimeout(() => process.exit(exitCode), 3000).unref();
  };

  process.on('SIGINT', () => shutdown('SIGINT', 130));
  process.on('SIGTERM', () => shutdown('SIGTERM', 143));

  uiDevChild.on('error', (error) => fail(`failed to start platform UI dev server: ${error.message}`));
  packageDevChild?.on('error', (error) => fail(`failed to start platform package dev server: ${error.message}`));

  const info = await waitForServer(
    uiDevChild,
    UI_DEV_SERVER_INFO_PATH,
    (serverInfo) => typeof serverInfo.baseUrl === 'string' ? serverInfo.baseUrl : null,
    'platform UI dev server',
  );
  const packageInfo = packageDevChild
    ? await waitForServer(
      packageDevChild,
      PACKAGE_DEV_SERVER_INFO_PATH,
      (serverInfo) => typeof serverInfo.indexUrl === 'string'
        ? serverInfo.indexUrl.replace(/\/index\.local\.json$/u, '')
        : null,
      'platform package dev server',
    )
    : null;
  console.log(`[tauri-test-ui] platform UI dev base: ${info.baseUrl}`);
  if (packageInfo?.indexUrl) {
    console.log(`[tauri-test-ui] platform package dev index: ${packageInfo.indexUrl}`);
  }
  if (packageInfo?.reloadUrl) {
    console.log(`[tauri-test-ui] platform package reload: ${packageInfo.reloadUrl}`);
  }
  console.log(`[tauri-test-ui] host mode: ${args.hostMode}`);
  testAppChild = startTestApp(args, info.baseUrl, packageInfo);
  testAppChild.on('error', (error) => fail(`failed to start Test app: ${error.message}`));
  testAppChild.on('exit', (code, signal) => {
    terminate(packageDevChild, signal || 'SIGTERM');
    terminate(uiDevChild, signal || 'SIGTERM');
    if (signal === 'SIGINT') process.exit(130);
    if (signal === 'SIGTERM') process.exit(143);
    process.exit(code ?? 0);
  });
}

main().catch((error) => fail(error.message));
