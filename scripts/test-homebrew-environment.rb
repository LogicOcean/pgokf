# Execute with `brew ruby` and an owned formula path (or a packaging tap).
# Ruby supplies actual Formula API observations; Rust owns their interpretation.
require "formula"
require "extend/ENV/shared"
require "json"
ENV.extend(SharedEnvExtension)
formula = Formulary.factory(ARGV.fetch(0))
formula.define_singleton_method(:buildpath) { formula.path.dirname/"build" }
formula.define_singleton_method(:system) { |*| throw :environment_ready, :environment_ready }
cases = [
  [nil, nil],
  ["--cfg=existing_plain", nil],
  ["--cfg=ignored_by_cargo", "--cfg=existing_encoded\x1f-Copt-level=1"],
  [nil, ""],
].map do |plain, encoded|
  ENV.delete("RUSTFLAGS")
  ENV.delete("CARGO_ENCODED_RUSTFLAGS")
  ENV["RUSTFLAGS"] = plain if plain
  ENV["CARGO_ENCODED_RUSTFLAGS"] = encoded if encoded
  ENV["HOMEBREW_RUSTFLAGS"] = "-Cdebuginfo=1"
  ENV["MACOSX_DEPLOYMENT_TARGET"] = "11.0"
  reached = catch(:environment_ready) { formula.install; :not_reached }
  {
    plain: plain, encoded: encoded,
    rustflags: ENV["RUSTFLAGS"], encoded_rustflags: ENV["CARGO_ENCODED_RUSTFLAGS"],
    homebrew_rustflags: ENV["HOMEBREW_RUSTFLAGS"],
    deployment_target: ENV["MACOSX_DEPLOYMENT_TARGET"], host_target: MacOS.version.to_s,
    status: reached.to_s,
  }
end
puts JSON.generate(cases)
