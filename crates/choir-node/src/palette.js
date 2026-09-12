/* The command palette: the search this surface already has, reachable
   from any page without finding the box first.
 *
 * WHAT THIS IS ALLOWED TO BE. D28 made the browser surface read-only and
 * script-free; D39 reversed that in one narrow place because a browser
 * write needs a signature a form cannot compute. This is the second
 * narrowing and it buys something smaller: no write, no new data, no
 * capability the page did not already have. Every result it draws is a
 * link the server-rendered `/search/` page draws too, and `⏎` on an
 * empty list goes to exactly that page. Turn scripting off and the box
 * in the bar still submits, which is the whole of the contract -- if
 * this file fails to load, nothing on the surface is worse than it was
 * before the file existed.
 *
 * NOTHING IS INTERPOLATED INTO IT. One response serves every reader and
 * every render, so it is cached once and carries no reader's state. What
 * it needs about the page it reads off the search form the chrome
 * already draws: `data-repo` and `data-rev`, or neither on a page that
 * is about no repository.
 *
 * EVERY VALUE FROM THE NETWORK GOES IN AS `textContent`. The API returns
 * repository paths and source lines, which are attacker-supplied by
 * definition on a node serving somebody else's push. Nothing here ever
 * assigns markup -- every node is built with `createElement` and filled
 * with `textContent` -- and the test for it greps this file for the
 * property that would break it, which is why the property is not named
 * here in a form that would match.
 */
(function () {
  "use strict";

  var bar = document.querySelector("form.omni");
  if (!bar || !window.HTMLDialogElement || !window.fetch || !window.AbortController) {
    return;
  }
  var field = bar.querySelector('input[name="q"]');
  var repo = bar.getAttribute("data-repo") || "";
  var rev = bar.getAttribute("data-rev") || "";
  var SCOPES = repo
    ? [["files", "files"], ["code", "code"], ["commits", "commits"]]
    : [["files", "repositories"]];
  var scope = repo ? "code" : "files";
  var LIMIT = 8;
  var DEBOUNCE = 140;

  /* ---- the frame, built once ------------------------------------- */

  var dialog = document.createElement("dialog");
  dialog.className = "palette";
  dialog.setAttribute("aria-label", "Search");

  var head = document.createElement("div");
  head.className = "palette-head";

  var input = document.createElement("input");
  input.type = "search";
  input.className = "palette-q";
  input.setAttribute("autocomplete", "off");
  input.setAttribute("spellcheck", "false");
  input.placeholder = repo ? "Search " + repo : "Search repositories";
  input.setAttribute("aria-label", input.placeholder);
  input.setAttribute("role", "combobox");
  input.setAttribute("aria-expanded", "true");
  input.setAttribute("aria-controls", "palette-list");
  head.appendChild(input);

  var tabs = document.createElement("div");
  tabs.className = "palette-scopes";
  var buttons = SCOPES.map(function (pair) {
    var button = document.createElement("button");
    button.type = "button";
    button.className = "palette-scope";
    button.textContent = pair[1];
    button.setAttribute("data-scope", pair[0]);
    button.addEventListener("click", function () {
      scope = pair[0];
      paint();
      run();
      input.focus();
    });
    tabs.appendChild(button);
    return button;
  });
  if (SCOPES.length > 1) head.appendChild(tabs);

  var list = document.createElement("ul");
  list.className = "palette-list";
  list.id = "palette-list";
  list.setAttribute("role", "listbox");

  var foot = document.createElement("p");
  foot.className = "palette-foot";
  foot.textContent = "↑↓ move · ↵ open · esc close";

  dialog.appendChild(head);
  dialog.appendChild(list);
  dialog.appendChild(foot);
  document.body.appendChild(dialog);

  /* ---- state ------------------------------------------------------ */

  var rows = [];
  var at = -1;
  var timer = 0;
  var flight = null;

  function paint() {
    buttons.forEach(function (button) {
      var on = button.getAttribute("data-scope") === scope;
      button.classList.toggle("here", on);
      button.setAttribute("aria-pressed", on ? "true" : "false");
    });
  }

  /* The page the form would have gone to: the palette's own last
     resort, and the thing `⏎` means when nothing is highlighted. */
  function full() {
    var url = bar.getAttribute("action") + "?q=" + encodeURIComponent(input.value.trim());
    if (repo) url += "&in=" + encodeURIComponent(scope);
    return url;
  }

  function say(text, className) {
    list.textContent = "";
    rows = [];
    at = -1;
    var li = document.createElement("li");
    li.className = className || "palette-note";
    li.textContent = text;
    list.appendChild(li);
  }

  function skeleton() {
    list.textContent = "";
    rows = [];
    at = -1;
    for (var n = 0; n < 3; n += 1) {
      var li = document.createElement("li");
      li.className = "palette-row skeleton";
      li.setAttribute("aria-hidden", "true");
      list.appendChild(li);
    }
  }

  function highlight(next) {
    if (!rows.length) return;
    at = (next + rows.length) % rows.length;
    rows.forEach(function (row, n) {
      row.classList.toggle("here", n === at);
      row.setAttribute("aria-selected", n === at ? "true" : "false");
    });
    rows[at].scrollIntoView({ block: "nearest" });
  }

  /* One result: a title, a place, and the href a click or `⏎` opens. */
  function add(href, title, place) {
    var li = document.createElement("li");
    li.className = "palette-row";
    li.setAttribute("role", "option");
    var link = document.createElement("a");
    link.href = href;
    var what = document.createElement("span");
    what.className = "what";
    what.textContent = title;
    link.appendChild(what);
    if (place) {
      var where = document.createElement("span");
      where.className = "where";
      where.textContent = place;
      link.appendChild(where);
    }
    li.appendChild(link);
    li.addEventListener("mousemove", function () {
      highlight(rows.indexOf(li));
    });
    list.appendChild(li);
    rows.push(li);
  }

  function blob(name, at_oid, path) {
    return "/r/" + name + "/blob/" + encodeURI(at_oid) + "/" + encodeURI(path);
  }

  function render(body) {
    list.textContent = "";
    rows = [];
    at = -1;
    (body.results || []).forEach(function (result) {
      var name = result.repo;
      (result.matches || []).forEach(function (match) {
        if (scope === "files") {
          // Node-wide, the repository is the fact a reader needs
          // second; inside one, they already know which it is.
          add(blob(name, rev || "HEAD", match), match, repo ? "" : name);
        } else if (scope === "code") {
          add(
            blob(name, rev || "HEAD", match.path),
            String(match.text || "").trim().slice(0, 160),
            match.path + ":" + match.line
          );
        } else {
          add(
            "/r/" + name + "/commit/" + encodeURI(match.oid),
            match.subject,
            String(match.oid).slice(0, 7) + " · " + match.author
          );
        }
      });
    });
    if (!rows.length) {
      say("No matches. ↵ runs the full search.");
      return;
    }
    if (body.truncated || body.matches > rows.length) {
      var li = document.createElement("li");
      li.className = "palette-row palette-more";
      var link = document.createElement("a");
      link.href = full();
      link.textContent = "All " + body.matches + " matches";
      li.appendChild(link);
      list.appendChild(li);
      rows.push(li);
    }
    highlight(0);
  }

  function run() {
    var q = input.value.trim();
    window.clearTimeout(timer);
    if (flight) flight.abort();
    if (!q) {
      say("Type to search " + (repo || "this node") + ".");
      return;
    }
    skeleton();
    timer = window.setTimeout(function () {
      var url = "/api/search?q=" + encodeURIComponent(q) + "&in=" + encodeURIComponent(scope) + "&limit=" + LIMIT;
      if (repo) url += "&repo=" + encodeURIComponent(repo) + "&rev=" + encodeURIComponent(rev);
      flight = new AbortController();
      window
        .fetch(url, { credentials: "same-origin", signal: flight.signal })
        .then(function (response) {
          if (!response.ok) throw new Error(String(response.status));
          return response.json();
        })
        .then(render)
        .catch(function (error) {
          if (error && error.name === "AbortError") return;
          say("Search is not answering. ↵ opens the full page.");
        });
    }, DEBOUNCE);
  }

  /* ---- opening and closing ---------------------------------------- */

  function open() {
    if (dialog.open) return;
    paint();
    input.value = field ? field.value : "";
    dialog.showModal();
    input.select();
    run();
  }

  dialog.addEventListener("close", function () {
    window.clearTimeout(timer);
    if (flight) flight.abort();
  });
  /* A click on the backdrop lands on the dialog itself, never on a
     child, which is the whole test for "outside". */
  dialog.addEventListener("click", function (event) {
    if (event.target === dialog) dialog.close();
  });
  input.addEventListener("input", run);
  input.addEventListener("keydown", function (event) {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      highlight(at + 1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      highlight(at - 1);
    } else if (event.key === "Enter") {
      event.preventDefault();
      var link = at >= 0 && rows[at] ? rows[at].querySelector("a") : null;
      window.location.assign(link ? link.getAttribute("href") : full());
    }
  });

  /* The one global listener. `/` is ignored wherever a character would
     otherwise be typed, including a `contenteditable` README a future
     page might carry. */
  document.addEventListener("keydown", function (event) {
    var typing =
      event.target &&
      (event.target.isContentEditable ||
        /^(INPUT|TEXTAREA|SELECT)$/.test(event.target.tagName || ""));
    if ((event.metaKey || event.ctrlKey) && !event.altKey && event.key.toLowerCase() === "k") {
      event.preventDefault();
      open();
    } else if (event.key === "/" && !typing && !event.metaKey && !event.ctrlKey) {
      event.preventDefault();
      open();
    }
  });

  /* The box in the bar becomes the way in rather than a second search:
     focusing it opens the palette with whatever is already typed. A
     reader who has scripting off never reaches this and submits the
     form, which is the same search one page later. */
  if (field) {
    field.addEventListener("focus", function () {
      open();
      field.blur();
    });
  }

  /* What a reader has to be told once: the key. Drawn into the box the
     server rendered, so it is never on a page the palette failed to
     reach. */
  var hint = document.createElement("kbd");
  hint.className = "omni-key";
  hint.textContent = /Mac|iPhone|iPad/.test(window.navigator.userAgent) ? "\u2318K" : "Ctrl K";
  bar.appendChild(hint);
})();
