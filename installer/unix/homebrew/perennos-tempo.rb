# DRAFT Homebrew formula for Perennos Tempo, source-available edition.
#
# NOT published. To use it: create a tap repository (for example Perennos/homebrew-tempo; a tap is a plain
# GitHub repo, free), put this file in its Formula/ folder, fill in url and sha256 from a real tag, then
#   brew install perennos/tempo/perennos-tempo
# It builds from the public repository (no Apple Developer ID needed: Homebrew builds are not quarantined).
# The full edition (the Tempo Field and the tempo command with VECTIS) is proprietary and is not in it.
class PerennosTempo < Formula
  desc "Local AI agent with Tempo Vault, Room and Studio as its tools (source-available edition)"
  homepage "https://perennos.com"
  url "https://github.com/Perennos/perennos-tempo/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_THE_SHA256_OF_THE_TAG_TARBALL"
  license "PolyForm-Shield-1.0.0"
  head "https://github.com/Perennos/perennos-tempo.git", branch: "main"

  depends_on "rust" => :build

  # every program must sit in the same folder: tempo-launch looks for the others next to itself
  CRATES = %w[
    perennos-tempo-launcher
    perennos-tempo-local-agent
    perennos-tempo-vault
    perennos-tempo-studio
    perennos-tempo-room
  ].freeze

  def install
    CRATES.each do |crate|
      system "cargo", "install", "--locked", "--root", prefix, "--path", "crates/#{crate}"
    end
    doc.install "installer/windows/legal/PolyForm-Shield-1.0.0.txt" => "LICENSE-PolyForm-Shield-1.0.0.txt"
    doc.install "installer/windows/legal/PolyForm-Shield-notice.txt" => "NOTICE.txt"
  end

  def caveats
    <<~EOS
      Start Perennos Tempo with:
        tempo-launch
      Hourly learning from your vault:
        tempo-launch schedule
      Your notes and Tempo's data are never touched by brew uninstall.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tempo-launch --version")
    assert_match "tempo-launch", shell_output("#{bin}/tempo-launch help")
    assert_path_exists bin/"tempo-vault"
  end
end
