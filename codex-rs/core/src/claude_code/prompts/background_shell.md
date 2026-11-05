You may specify an optional timeout in milliseconds (up to 600000ms / 10 minutes for a foreground command). By default, your command will timeout after 120000ms (2 minutes).

You can use the `run_in_background` parameter to run the command in the background. Only use this if you don't need the result immediately and are OK being notified when the command completes later. You do not need to check the output right away - you'll be notified when it finishes. You do not need to use '&' at the end of the command when using this parameter. With `run_in_background` the timeout is instead how long the command may run in the background (default 1800000ms / 30 minutes, max 7200000ms / 2 hours); at that limit it is stopped and you are notified.

Use the Monitor tool to stream events from a background process (each stdout line is a notification). For one-shot "wait until done," use Bash with run_in_background instead.

If waiting for a background task you started with `run_in_background`, you will be notified when it completes — do not poll.
