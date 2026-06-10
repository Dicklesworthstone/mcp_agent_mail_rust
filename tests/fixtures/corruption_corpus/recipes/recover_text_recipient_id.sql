CREATE TABLE message_recipients(
  id INTEGER PRIMARY KEY,
  message_id INTEGER NOT NULL,
  agent_id INTEGER NOT NULL,
  kind TEXT NOT NULL,
  read_ts INTEGER,
  ack_ts INTEGER
);
INSERT INTO message_recipients(id,message_id,agent_id,kind,read_ts,ack_ts)
VALUES('text-recipient-id',1,1,'to',NULL,NULL);
