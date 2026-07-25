# Homebrew formula template / reference for Albatross.
#
# Publishing is AUTOMATED. On every `vX.Y.Z` tag, the release workflow
# (.github/workflows/release.yml) builds the tarballs, creates the GitHub
# release, and then regenerates `Formula/albatross.rb` in the
# `morganlinton/homebrew-tap` repo with the new version + checksums and pushes
# it. That step authenticates with a `TAP_GITHUB_TOKEN` repo secret — a
# fine-grained PAT with `contents: write` on morganlinton/homebrew-tap.
# Without the secret the step is skipped and the release still publishes.
#
# This file is kept as a reference and manual fallback: to bump the tap by
# hand, copy it into the tap repo and set `version` + the two `sha256` values
# from the release's SHA256SUMS. Users install with
# `brew install morganlinton/tap/albatross`.
#
# Notes:
#   - This formula installs a pre-built binary, not a Cargo build, so users
#     don't need a Rust toolchain to install.
#   - If you decide later to submit to homebrew-core instead, the formula
#     shape mostly carries over; homebrew-core may require a from-source
#     build path as well.

class Albatross < Formula
  desc "Terminal-based agent harness for running small LLMs on your Mac"
  homepage "https://github.com/morganlinton/Albatross"
  version "0.3.0" # bump per release
  license "MIT"

  ARM64_SHA256 = "0000000000000000000000000000000000000000000000000000000000000000".freeze
  X86_64_SHA256 = "0000000000000000000000000000000000000000000000000000000000000000".freeze

  on_macos do
    on_arm do
      url "https://github.com/morganlinton/Albatross/releases/download/v#{version}/albatross-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 ARM64_SHA256
    end
    on_intel do
      url "https://github.com/morganlinton/Albatross/releases/download/v#{version}/albatross-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 X86_64_SHA256
    end
  end

  def install
    bin.install "albatross"
    # README/LICENSE/Quickstart are bundled in the tarball — install them
    # under doc/ so `brew info` and `brew docs` show something useful.
    doc.install Dir["README.md", "Quickstart.md", "LICENSE"]

    # Best-effort shell completion install. `albatross completions <shell>`
    # writes the script to stdout; pipe it into the standard locations
    # Homebrew already manages.
    generate_completions_from_executable(bin/"albatross", "completions")
  end

  test do
    assert_match "albatross", shell_output("#{bin}/albatross completions bash")
  end
end
