class Orch < Formula
  desc "Multi-agent task orchestrator for AI coding agents (claude, codex, opencode)"
  homepage "https://github.com/gabrielkoerich/orchestrator"
  url "https://github.com/gabrielkoerich/orchestrator/archive/refs/tags/v1.0.0-alpha.1.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  head "https://github.com/gabrielkoerich/orchestrator.git", branch: "main"
  license "MIT"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args

    # Install shell scripts (still used by run_task.sh in phase 2)
    libexec.install "scripts" if (buildpath/"scripts").exist?
    libexec.install "prompts" if (buildpath/"prompts").exist?
    libexec.install "justfile" if (buildpath/"justfile").exist?
    libexec.install Dir["*.example.yml"]
    libexec.install "skills.yml" if (buildpath/"skills.yml").exist?
  end

  service do
    run [opt_bin/"orch-core", "serve"]
    keep_alive true
    log_path var/"log/orch.log"
    error_log_path var/"log/orch.error.log"
  end

  def caveats
    <<~EOS
      To get started:
        cd ~/your-project
        orch-core init                # configure project
        orch-core task add "title"    # add a task
        brew services start orch      # start background server

      Required agent CLIs (install at least one):
        brew install --cask claude-code   # Claude
        brew install --cask codex         # Codex
        brew install opencode             # OpenCode

      Optional for GitHub sync:
        brew install gh && gh auth login
    EOS
  end

  test do
    assert_match "orch-core", shell_output("#{bin}/orch-core --version 2>&1", 0)
  end
end
