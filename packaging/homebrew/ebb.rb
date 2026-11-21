cask "ebb" do
  version "26.3"
  # Replace :no_check with the DMG sha256 from SHA256SUMS.txt (populated automatically in dist/ebb.rb on release).
  sha256 :no_check

  url "https://github.com/minhntothex/ebb/releases/download/#{version}/ebb-#{version}-macos-universal.dmg"
  name "Ebb"
  desc "Minimal macOS Menu Bar monitor for Claude Code, OpenAI Codex, and Antigravity CLI quota"
  homepage "https://github.com/minhntothex/ebb"

  depends_on macos: ">= :ventura"

  app "Ebb.app"

  caveats <<~EOS
    Ebb.app is ad-hoc signed. If macOS Gatekeeper blocks the first launch, run:
      xattr -dr com.apple.quarantine /Applications/Ebb.app

    To enable Claude Code or Antigravity CLI (agy) quota updates (opt-in, preserves existing statusLine):
      /Applications/Ebb.app/Contents/MacOS/ebb setup-claude-statusline --apply
      /Applications/Ebb.app/Contents/MacOS/ebb setup-agy-statusline --apply
  EOS

  zap trash: [
    "~/Library/Application Support/Ebb",
    "~/Library/LaunchAgents/com.minhntothex.ebb.plist",
  ]
end
