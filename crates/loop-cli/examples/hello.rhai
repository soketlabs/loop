// Example Loop extension (copy to ~/.loop/agent/extensions/hello.rhai)

register_command("hello", "Say hello from a Rhai extension");
notify("hello extension loaded");
