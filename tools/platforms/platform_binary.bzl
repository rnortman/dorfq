"""A binary built for another platform, published under a fixed name."""

def _platform_impl(_settings, attr):
    return {"//command_line_option:platforms": str(attr.platform)}

_platform = transition(
    implementation = _platform_impl,
    inputs = [],
    outputs = ["//command_line_option:platforms"],
)

def _platform_binary_impl(ctx):
    binary = ctx.executable.binary
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(output = out, target_file = binary, is_executable = True)
    return [DefaultInfo(
        files = depset([out]),
        executable = out,
        runfiles = ctx.runfiles([out, binary]),
    )]

platform_binary = rule(
    implementation = _platform_binary_impl,
    doc = "Builds `binary` for `platform` and puts it at `name` in this " +
          "package, so it has a fixed path under bazel-bin.",
    attrs = {
        "binary": attr.label(
            cfg = _platform,
            executable = True,
            mandatory = True,
            doc = "The rust_binary to build for `platform`.",
        ),
        "platform": attr.label(
            mandatory = True,
            doc = "The target platform.",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
    executable = True,
)
