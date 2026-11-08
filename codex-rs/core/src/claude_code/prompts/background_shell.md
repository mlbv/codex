`yield_time_ms` controls the foreground wait, with the normal 10000 ms default and platform-specific bounds. A command still running when that wait ends continues as a background task and sends a completion notification automatically.

Use `run_in_background = true` to return after command admission when you can continue useful work without its result. You do not need '&' at the end of the command. `timeout_ms` sets the background lifetime: default 1800000 ms / 30 minutes (600000 ms / 10 minutes in noninteractive sessions), maximum 7200000 ms / 2 hours. At that limit the command is stopped and you are notified. The lifetime starts at admission for explicit background execution, or when a foreground command yields.

Use `monitor` to stream events from a background process. For a single completion notification, use `exec_command`. Use `write_stdin` for terminal input, `task_output` for retained output, and `task_stop` to stop a task.

If waiting for a background task you started with `run_in_background`, you will be notified when it completes — do not poll.
