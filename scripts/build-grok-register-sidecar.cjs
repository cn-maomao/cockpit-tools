const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const sidecarDir = path.join(root, 'sidecars', 'grok-register');
const args = new Set(process.argv.slice(2));
const targetArg = process.argv.find((value) => value.startsWith('--target='));
const target = targetArg?.slice('--target='.length) || process.env.TAURI_ENV_TARGET_TRIPLE;
if (!target) {
  throw new Error('Missing --target=<rust-target> or TAURI_ENV_TARGET_TRIPLE');
}

const candidates = process.env.PYTHON
  ? [process.env.PYTHON]
  : process.platform === 'win32'
    ? ['python', 'py']
    : ['python3', 'python'];

let python = null;
for (const candidate of candidates) {
  const probe = spawnSync(candidate, ['--version'], { stdio: 'ignore' });
  if (probe.status === 0) {
    python = candidate;
    break;
  }
}
if (!python) throw new Error('Python 3.10+ is required to build cockpit-grok-register');

function run(commandArgs) {
  const targetArch = target.startsWith('aarch64-apple')
    ? 'arm64'
    : target.startsWith('x86_64-apple')
      ? 'x86_64'
      : target.startsWith('universal-apple')
        ? 'universal2'
        : '';
  const result = spawnSync(python, commandArgs, {
    cwd: sidecarDir,
    stdio: 'inherit',
    env: { ...process.env, PYINSTALLER_TARGET_ARCH: targetArch },
  });
  if (result.status !== 0) process.exit(result.status ?? 1);
}

if (args.has('--install')) {
  run(['-m', 'pip', 'install', '--disable-pip-version-check', '-r', 'requirements-build.txt']);
}

const binDir = path.join(sidecarDir, 'bin');
const workDir = path.join(root, 'target', 'grok-register-pyinstaller', target);
fs.mkdirSync(binDir, { recursive: true });
fs.mkdirSync(workDir, { recursive: true });
run([
  '-m', 'PyInstaller', '--noconfirm', '--clean',
  '--distpath', binDir, '--workpath', workDir,
  'cockpit-register.spec',
]);

const extension = process.platform === 'win32' ? '.exe' : '';
const source = path.join(binDir, `cockpit-grok-register${extension}`);
const destination = path.join(binDir, `cockpit-grok-register-${target}${extension}`);
if (fs.existsSync(destination)) fs.rmSync(destination);
fs.renameSync(source, destination);
console.log(`Built ${destination}`);
