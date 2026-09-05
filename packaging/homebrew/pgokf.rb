# Homebrew formula for the pgokf PostgreSQL extension.
#
# Intended for a tap, e.g. LogicOcean/homebrew-pgokf:
#
#   brew tap logicocean/pgokf
#   brew install pgokf
#
# Builds the extension from source against Homebrew's PostgreSQL and installs
# the .so + control + sql into the Homebrew postgresql keg so
# `CREATE EXTENSION pgokf;` works on a `brew services`-managed cluster.
class Pgokf < Formula
  desc "Materialized PostgreSQL catalog for Open Knowledge Format bundles"
  homepage "https://github.com/LogicOcean/pgokf"
  # The tag tracks default_version in crates/extension/pgokf.control, the single
  # source of truth for the extension version; bump the url, the sha256 below,
  # and the version assertions in `test do` together.
  url "https://github.com/LogicOcean/pgokf/archive/refs/tags/v0.1.14.tar.gz"
  # Digest of the GitHub-generated tag tarball. Regenerate on every release with:
  #   curl -fsSL https://github.com/LogicOcean/pgokf/archive/refs/tags/vX.Y.Z.tar.gz | shasum -a 256
  sha256 "4efc3142b300a310cec761cb5c6ce9d56fb81072ab04bb3868f4256e6602ca2b"
  license "AGPL-3.0-only"
  head "https://github.com/LogicOcean/pgokf.git", branch: "main"

  # Track Homebrew's current default PostgreSQL. The formula is written to
  # follow whatever major `postgresql@17` resolves to; bump this pair together.
  depends_on "rust" => :build
  depends_on "postgresql@17"

  def postgresql
    deps.map(&:to_formula).find { |f| f.name.start_with?("postgresql@") }
  end

  def install
    pg = postgresql
    pg_config = pg.opt_bin/"pg_config"
    pg_major = pg.version.major.to_s

    # cargo-pgrx is the build driver; pin it to the workspace pgrx version and
    # keep it inside the build sandbox so it never touches ~/.cargo.
    ENV["CARGO_HOME"] = buildpath/"cargo-home"
    system "cargo", "install", "cargo-pgrx",
           "--version", "0.19.2", "--locked",
           "--root", buildpath/"pgrx-tools"
    cargo_pgrx = buildpath/"pgrx-tools/bin/cargo-pgrx"

    system cargo_pgrx, "pgrx", "init", "--pg#{pg_major}=#{pg_config}"

    cd "crates/extension" do
      system cargo_pgrx, "pgrx", "package",
             "--no-default-features", "--features", "pg#{pg_major}",
             "--pg-config", pg_config
    end

    # cargo pgrx package writes a filesystem image mirroring the target root
    # under target/release/pgokf-pg<major>/. pg_config's dirs live inside the
    # postgresql keg, so translate that absolute image into this formula's
    # prefix and let `brew link` expose it in the keg.
    staged = buildpath/"target/release/pgokf-pg#{pg_major}"
    libdir = Pathname.new(Utils.safe_popen_read(pg_config, "--pkglibdir").strip)
    sharedir = Pathname.new(Utils.safe_popen_read(pg_config, "--sharedir").strip)

    (prefix/libdir.relative_path_from(HOMEBREW_PREFIX)).install \
      staged/"#{libdir.relative_path_from(Pathname.new("/"))}/pgokf.so"

    ext_dst = prefix/sharedir.relative_path_from(HOMEBREW_PREFIX)/"extension"
    ext_src = staged/"#{sharedir.relative_path_from(Pathname.new("/"))}/extension"
    ext_dst.install Dir["#{ext_src}/pgokf*"]
  end

  def caveats
    <<~EOS
      pgokf was installed into the Homebrew PostgreSQL keg. In a database on a
      running cluster (`brew services start postgresql@17`), enable it with:

        CREATE EXTENSION pgokf;

      Registering an OKF bundle requires a server-readable absolute bundle path
      and membership in pgokf_writer (reader < writer < admin). See:
        https://github.com/LogicOcean/pgokf#quick-start
    EOS
  end

  test do
    pg = postgresql
    pg_major = pg.version.major.to_s

    # The control file must be discoverable in the extension sharedir.
    control = share/"postgresql@#{pg_major}/extension/pgokf.control"
    control = share/"postgresql/extension/pgokf.control" unless control.exist?
    assert_predicate control, :exist?, "pgokf.control not installed"
    assert_match "default_version = '0.1.14'", control.read

    # End-to-end: initialize a throwaway cluster and CREATE EXTENSION.
    pg_bin = pg.opt_bin
    datadir = testpath/"data"
    system pg_bin/"initdb", "-D", datadir, "--auth=trust", "-U", "postgres"
    port = free_port
    system pg_bin/"pg_ctl", "-D", datadir, "-w",
           "-o", "-p #{port} -c listen_addresses=127.0.0.1", "start"
    begin
      output = shell_output(
        "#{pg_bin}/psql -h 127.0.0.1 -p #{port} -U postgres -d postgres " \
        "-tAc 'CREATE EXTENSION pgokf; SELECT extversion FROM pg_extension WHERE extname=''pgokf'';'",
      )
      assert_match "0.1.14", output
    ensure
      system pg_bin/"pg_ctl", "-D", datadir, "-w", "stop"
    end
  end
end
