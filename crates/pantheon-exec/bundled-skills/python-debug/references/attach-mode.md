# debugpy attach mode

For processes you cannot just rerun under pdb: servers, workers, anything
long-lived. The pattern is: the target opens a listener, your script
connects and drives it. Still non-interactive.

## Setup

Install debugpy where the target runs:

```
pip install debugpy
```

In the target process, before the code you want to inspect:

```python
import debugpy
debugpy.listen(("0.0.0.0", 5678))
# debugpy.wait_for_client()  # uncomment to pause until you attach
debugpy.breakpoint()          # hard breakpoint wherever you need one
```

Security note: `listen` opens a port with full code-execution access to
the process. Bind to `127.0.0.1` unless you have a reason not to, use a
non-obvious port, and never leave it on in production.

## Attaching from a script

Use debugpy's client API in a script (run with the sandbox's python3):

```python
import debugpy

debugpy.connect(("target-host", 5678))
# execution continues in the target; breakpoints hit there will pause it.
# Inspect via logging or by evaluating expressions through a second
# debugpy.breakpoint() site that prints state, e.g.:
#   debugpy.breakpoint()
# with prior: import pprint; pprint.pprint({"queue_depth": q.qsize()})
```

There is no full interactive REPL over this connection from a script.
The practical pattern: place `debugpy.breakpoint()` at the site of
interest with print/pprint of the state you need just before it, attach,
trigger the code path, read the target's stdout.

## Cleanup

Remove `debugpy.listen`, `debugpy.connect`, and `debugpy.breakpoint()`
calls before the code ships. They are debugging scaffolding with a
network port attached.
