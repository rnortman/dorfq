"""The report site's WebAssembly module, built for wasm32 and optimised."""

def _wasm_opt_impl(_settings, _attr):
    return {
        "//command_line_option:compilation_mode": "opt",
        "//command_line_option:platforms": "@rules_rust//rust/platform:wasm32",
    }

# Every invocation builds the module the same way: for wasm32, and at `opt`
# whatever mode the invoker is in, its dependencies included. The page runs
# it in a browser, where an unoptimised simulator is an order of magnitude
# slower.
_wasm_opt = transition(
    implementation = _wasm_opt_impl,
    inputs = [],
    outputs = [
        "//command_line_option:compilation_mode",
        "//command_line_option:platforms",
    ],
)

def _wasm_module_impl(ctx):
    modules = [
        f
        for dep in ctx.attr.deps
        for f in dep[DefaultInfo].files.to_list()
        if f.extension == "wasm"
    ]
    if len(modules) != 1:
        fail("expected exactly one .wasm among the deps' files, found %s" % modules)
    out = ctx.actions.declare_file(ctx.attr.out)
    ctx.actions.symlink(output = out, target_file = modules[0])
    return [DefaultInfo(files = depset([out]))]

wasm_module = rule(
    implementation = _wasm_module_impl,
    doc = "Builds `deps` for wasm32 at `opt` and puts its one .wasm at `out` " +
          "in this package, so it has a fixed path under bazel-bin.",
    attrs = {
        "deps": attr.label_list(
            cfg = _wasm_opt,
            mandatory = True,
            doc = "The cdylib whose .wasm this rule publishes.",
        ),
        "out": attr.string(
            mandatory = True,
            doc = "The module's file name in this package.",
        ),
    },
)
