cask "cockpit-tools" do
  version "1.3.8"
  sha256 "9741861e22a3745a51e60e3b6c4d5faf13df1e8623828c1c91364d22d15d1137"

  url "https://github.com/cn-maomao/cockpit-tools/releases/download/v#{version}/Cockpit.Tools_#{version}_universal.dmg",
      verified: "github.com/cn-maomao/cockpit-tools/"
  name "Cockpit Tools"
  desc "Account manager for AI IDEs (Antigravity and Codex)"
  homepage "https://github.com/cn-maomao/cockpit-tools"

  auto_updates true

  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-cr", "#{appdir}/Cockpit Tools.app"],
                   sudo: true
  end

  app "Cockpit Tools.app"

  zap trash: [
    "~/Library/Application Support/com.jlcodes.cockpit-tools",
    "~/Library/Caches/com.jlcodes.cockpit-tools",
    "~/Library/Preferences/com.jlcodes.cockpit-tools.plist",
    "~/Library/Saved Application State/com.jlcodes.cockpit-tools.savedState",
  ]

  caveats <<~EOS
    The app is automatically quarantined by macOS. A postflight hook has been added to remove this quarantine.
    If you still encounter the "App is damaged" error, please run:
      sudo xattr -rd com.apple.quarantine "/Applications/Cockpit Tools.app"
  EOS
end
