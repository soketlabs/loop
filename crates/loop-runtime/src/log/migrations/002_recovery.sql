-- Recovery reads only from the newest generation whose recovery committed.
ALTER TABLE runtime_generations ADD COLUMN recovered_ms INTEGER;
CREATE INDEX events_generation ON events(generation, global_seq);
