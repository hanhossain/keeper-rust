create table players
(
    id            text primary key,
    first_name    text,
    last_name     text,
    active        bool,
    position      text,
    team          text,
    status        text,
    injury_status text
);
