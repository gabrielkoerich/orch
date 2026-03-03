class Orch < Formula
  desc "Multi-agent task orchestrator for AI coding agents (claude, codex, opencode)"
  homepage "https://github.com/gabrielkoerich/orch"
  # This local template is intentionally simple: prefer a single universal binary when
  # possible. The CI release job will generate a formula with either a universal
  # `url`/`sha256` or an arch-specific `on_macos` block depending on uploaded artifacts.
  version "VERSION_PLACEHOLDER"
  license "MIT"

  url "UNIVERSAL_URL_PLACEHOLDER"
  sha256 "UNIVERSAL_SHA256_PLACEHOLDER"

  depends_on "tmux"

  def install
    bin.install "orch" => "orch"

    # Install additional resources if present in the tarball
    (libexec/"prompts").install Dir["prompts/*"] if (buildpath/"prompts").exist?
    libexec.install Dir["*.example.yml"] if Dir.glob("*.example.yml").any?
    libexec.install "skills.yml" if (buildpath/"skills.yml").exist?
  end

  service do
    run [opt_bin/"orch", "serve"]
    keep_alive true
    log_path var/"log/orch.log"
    error_log_path var/"log/orch.error.log"
  end

  def caveats
    <<~EOS
      To get started:
        cd ~/your-project
        orch init                     # configure project
        orch task add "title"         # add a task
        brew services start orch      # start background server

      Required agent CLIs (install at least one):
        brew install --cask claude-code   # Claude
        brew install --cask codex         # Codex
        brew install opencode             # OpenCode

      GitHub authentication:
        Set a Personal Access Token in `GH_TOKEN`/`GITHUB_TOKEN` or run `orch auth check` to verify your configuration.
        # Interactive `gh auth login` is supported as a legacy option but is not required by this formula.
    EOS
  end

  test do
    assert_match "orch", shell_output("#{bin}/orch --version 2>&1", 0)
  end
end
