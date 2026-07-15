class Nudge < Formula
  desc "Rate-limit auto-resumer for AI CLIs in tmux"
  homepage "https://github.com/Davey-Hughes/daveyutils"
  license "GPL-3.0-or-later"
  head "https://github.com/Davey-Hughes/daveyutils.git", branch: "main"

  # On the first tagged release, add:
  #   url "https://github.com/Davey-Hughes/daveyutils/archive/refs/tags/nudge-v0.1.0.tar.gz"
  #   sha256 "..."

  depends_on "rust" => :build
  depends_on "tmux"

  def install
    cd "nudge-rs" do
      system "cargo", "install", "--locked", "--root", prefix, "--path", "."
      generate_completions_from_executable(bin/"nudge", "--completions", shells: [:bash, :zsh, :fish])
    end
  end

  test do
    assert_match "nudge", shell_output("#{bin}/nudge --version")
  end
end
