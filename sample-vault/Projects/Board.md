# Project board

Views are saved queries over note properties. They're fenced code blocks, so this file is still plain Markdown everywhere else. Drag a card by its handle to another column to change its `status`.

```quire-view
from: "#project"
layout: kanban
group: status
columns: [planned, active, done]
fields: [title, due, owner]
```

## Coming up

```quire-view
from: "#project"
where: status != done
sort: due asc
layout: table
fields: [title, status, due, owner]
```

## Due dates

```quire-view
from: "#project"
layout: calendar
date: due
```
