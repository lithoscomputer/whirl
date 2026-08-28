# Formula template for the Homebrew tap (lithoscomputer/homebrew-tap).
# On each release, copy this into the tap as Formula/whirl.rb with the
# version and the sha256 values from the release's .sha256 assets filled
# in. See RELEASING.md.
class Whirl < Formula
  desc "Run web UI tests written in plain text files"
  homepage "https://github.com/lithoscomputer/whirl"
  version "0.1.0"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/lithoscomputer/whirl/releases/download/v#{version}/whirl-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_MACOS_ARM64_SHA256"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/lithoscomputer/whirl/releases/download/v#{version}/whirl-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_LINUX_X86_64_SHA256"
    end
    on_arm do
      url "https://github.com/lithoscomputer/whirl/releases/download/v#{version}/whirl-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_LINUX_ARM64_SHA256"
    end
  end

  def install
    bin.install "whirl"
  end

  def caveats
    <<~EOS
      Provision the browser runtime before the first run:
        whirl install
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/whirl --version")
  end
end
