version: 3
kind: run
meta:
  id: {run_id}
  name: {name}
  description: TODO describe run purpose.
environment_ref: {environment}
scenario_ref: {scenario_ref}
execution:
  mode: workflow
  target: {target}
assertions:
  mode: strict
