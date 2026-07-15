# -*- mode: python ; coding: utf-8 -*-
import os
from PyInstaller.utils.hooks import collect_all

datas = [('assets', 'assets')]
binaries = []
hiddenimports = []
for package in ('DrissionPage', 'curl_cffi', 'tldextract', 'DataRecorder', 'DownloadKit'):
    package_datas, package_binaries, package_hiddenimports = collect_all(package)
    datas += package_datas
    binaries += package_binaries
    hiddenimports += package_hiddenimports

analysis = Analysis(
    ['cockpit_sidecar.py'],
    pathex=['.'],
    binaries=binaries,
    datas=datas,
    hiddenimports=hiddenimports,
    noarchive=False,
)
pyz = PYZ(analysis.pure)
exe = EXE(
    pyz,
    analysis.scripts,
    analysis.binaries,
    analysis.datas,
    [],
    name='cockpit-grok-register',
    console=True,
    strip=False,
    upx=False,
    target_arch=os.environ.get('PYINSTALLER_TARGET_ARCH') or None,
)
