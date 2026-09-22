# Execute with `brew ruby` and a formula in a disposable local tap.
raise "macOS required" unless RUBY_PLATFORM.include?("darwin")
require "formula"
require "extend/ENV/shared"
ENV.extend(SharedEnvExtension)
formula = Formulary.factory(ARGV.fetch(0))
# Run the actual install method until its first external command. No build or
# installation is substituted in the live gate; this separately probes flag
# composition with the real Homebrew environment API.
formula.define_singleton_method(:buildpath) { Pathname.new("/tmp/pgokf-env-probe") }
formula.define_singleton_method(:system) { |*| throw :environment_ready }
[
  [nil, nil],
  ["--cfg=existing_plain", nil],
  ["--cfg=ignored_by_cargo", "--cfg=existing_encoded\x1f-Copt-level=1"],
  [nil, ""],
].each do |plain, encoded|
  ENV.delete("RUSTFLAGS")
  ENV.delete("CARGO_ENCODED_RUSTFLAGS")
  ENV["RUSTFLAGS"] = plain if plain
  ENV["CARGO_ENCODED_RUSTFLAGS"] = encoded if encoded
  ENV["HOMEBREW_RUSTFLAGS"] = "-Cdebuginfo=1"
  ENV["MACOSX_DEPLOYMENT_TARGET"] = "11.0"
  reached = catch(:environment_ready) { formula.install; :not_reached }
  raise "install command never executed" if reached == :not_reached
  raise "host deployment target lost" unless ENV.fetch("MACOSX_DEPLOYMENT_TARGET") == MacOS.version.to_s
  raise "Homebrew flags lost" unless ENV.fetch("HOMEBREW_RUSTFLAGS") == "-Cdebuginfo=1"
  if encoded
    expected = [encoded, "-Clink-arg=-Wl,-undefined,dynamic_lookup"].reject(&:empty?).join("\x1f")
    raise "encoded flags lost" unless ENV.fetch("CARGO_ENCODED_RUSTFLAGS") == expected
    raise "plain flags changed" unless ENV["RUSTFLAGS"] == plain
  else
    expected = [plain, "-C link-arg=-Wl,-undefined,dynamic_lookup"].compact.join(" ")
    raise "plain flags lost" unless ENV.fetch("RUSTFLAGS") == expected
  end
end
puts "PASS: four actual formula environment composition cases"
