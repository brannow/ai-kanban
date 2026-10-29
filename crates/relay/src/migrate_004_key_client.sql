-- Version 4: what each key's machine last said it runs, from its `User-Agent`
-- (`ai-kanban/0.2.0 (protocol 1)`). So the admin can see, in `key list`, who would be stopped
-- before raising the protocol a relay requires. NULL until a request has said.
ALTER TABLE keys ADD COLUMN client TEXT;
