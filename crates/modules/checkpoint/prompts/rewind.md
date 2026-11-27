End the active checkpoint: the files return to the snapshot (files created since are removed),
the conversation returns to the checkpoint, and `report` is kept as the next message. Pass
`checkpoint` to return to another snapshot instead (an id from `hrn checkpoints list`). The
replaced state is snapshotted first, so `hrn checkpoints restore` can undo it.
