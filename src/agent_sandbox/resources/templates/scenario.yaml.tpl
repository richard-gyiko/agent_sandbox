version: 3
kind: scenario
meta:
  id: {scenario_id}
  name: {name}
  description: TODO describe the scenario intent.
  tags: [p0, strict]
seed:
  gmail:
    messages: []
  drive:
    folders:
      - id: root
        name: root
        parent_id: null
    files: []
expect:
  mode: strict
  assertions: []
