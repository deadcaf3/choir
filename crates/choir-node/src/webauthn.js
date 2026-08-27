// The whole client half of D39, as one same-origin file.
//
// It was three inline constants until the CSP moved to `script-src
// 'self'`. What D28 and D39 were defending — no third party, no library,
// no build step, nothing off this origin, nothing that breaks offline —
// all survives; what changed is that the bytes arrive as a resource this
// node serves rather than as a copy pasted into every page that needs
// them, so the header can name a source instead of three digests.
//
// Three ceremonies live here, and each one starts by asking whether its
// own element is on the page. That is what lets a single file serve the
// review page and the account page without either of them running the
// other's code: the account page has no `#verdict`, the review page has
// no `#enrol`, and a page with neither runs nothing at all.
//
// The node renders every payload and every challenge. This file hands a
// challenge to the authenticator and posts back what comes out. It does
// not know the op format, and that is deliberate: the alternative is a
// second implementation of a hashed format in a language with no tests
// in this workspace.
(function () {
  'use strict';

  // A browser without WebAuthn keeps the server-rendered page and its
  // `<noscript>` sentence naming the CLI, which is the whole fallback.
  // Checked at the bottom rather than here, because D72's ask ceremony
  // needs no authenticator: it runs on a browser that has never heard of
  // a passkey, which is most of the browsers that reach the front door.
  var webauthn = !!(window.PublicKeyCredential && navigator.credentials);

  var hex = function (buf) {
    return Array.prototype.map.call(new Uint8Array(buf), function (b) {
      return ('0' + b.toString(16)).slice(-2);
    }).join('');
  };

  var unb64url = function (s) {
    var t = s.replace(/-/g, '+').replace(/_/g, '/');
    var raw = atob(t + '==='.slice(0, (4 - t.length % 4) % 4));
    var out = new Uint8Array(raw.length);
    for (var i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
    return out;
  };

  var b64url = function (buf) {
    var s = '';
    var b = new Uint8Array(buf);
    for (var i = 0; i < b.length; i++) s += String.fromCharCode(b[i]);
    return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  };

  // Every ceremony reports into a paragraph that starts hidden, so the
  // reader is never shown an empty status line before they act.
  var sayer = function (id) {
    var el = document.getElementById(id);
    return function (text) {
      if (!el) return;
      el.hidden = false;
      el.textContent = text;
    };
  };

  // The one shape all three share: a promise ending in a node response.
  // A `2xx` means the op is in the log, so the page is reloaded rather
  // than patched — the server-rendered document is the truth, and
  // rewriting it here is how a page starts needing script to say what it
  // knows.
  var settle = function (promise, say, refused, failed) {
    return promise.then(function (r) {
      if (r.ok) { location.reload(); return; }
      return r.text().then(function (t) { say(refused + t); });
    }).catch(function (e) { say(failed + e.message); });
  };

  var post = function (path, body) {
    return fetch(path, {
      method: 'POST',
      credentials: 'same-origin',
      body: JSON.stringify(body)
    });
  };

  // A signed submission, assembled from what the authenticator returned.
  // Scheme 2 is WebAuthn/ES256, the one scheme the node verifies.
  var submit = function (channel, payloadHex, credential) {
    return post('/api/submit', {
      channel: channel,
      payload_hex: payloadHex,
      key_id: credential.id,
      scheme: 2,
      signature_hex: hex(credential.response.signature),
      authenticator_data_hex: hex(credential.response.authenticatorData),
      client_data_json_hex: hex(credential.response.clientDataJSON)
    });
  };

  var assert = function (challenge) {
    return navigator.credentials.get({
      publicKey: { challenge: unb64url(challenge), userVerification: 'preferred' }
    });
  };

  // Casting a verdict. The payload and its challenge are rendered by the
  // node onto each button, so nothing here composes an op.
  var verdict = function () {
    var box = document.getElementById('verdict');
    if (!box) return;
    var say = sayer('verdict-said');
    box.hidden = false;
    Array.prototype.forEach.call(document.querySelectorAll('button.verdict'), function (b) {
      b.addEventListener('click', function () {
        say('Waiting for your authenticator...');
        settle(
          assert(b.dataset.challenge).then(function (c) {
            return submit(box.dataset.user, b.dataset.payload, c);
          }),
          say,
          'The node refused it: ',
          'Not signed: '
        );
      });
    });
  };

  // Commenting. Unlike a verdict the payload cannot be rendered ahead of
  // time — it carries text nobody has typed yet — so this asks
  // `/api/prepare` for one and signs what comes back. The op format
  // stays in a single implementation either way.
  var comment = function () {
    var box = document.getElementById('comment');
    var go = document.getElementById('comment-go');
    if (!box || !go) return;
    var say = sayer('comment-said');
    box.hidden = false;
    go.addEventListener('click', function () {
      var text = document.getElementById('comment-body').value;
      if (!text.trim()) { say('Nothing to say yet.'); return; }
      say('Preparing...');
      settle(
        post('/api/prepare', { kind: 'comment', id: box.dataset.review, body: text })
          .then(function (r) {
            if (!r.ok) return r.text().then(function (t) { throw new Error(t); });
            return r.json();
          })
          .then(function (p) {
            say('Waiting for your authenticator...');
            return assert(p.challenge).then(function (c) {
              return submit(p.channel, p.payload_hex, c);
            });
          }),
        say,
        'The node refused it: ',
        'Not posted: '
      );
    });
  };

  // Enrolling a passkey.
  //
  // The challenge here is not verified by the node, and that is stated
  // rather than hidden: WebAuthn requires one, so one is sent, but
  // nothing parses the attestation object so nothing checks it came
  // back. D39 scoped a CBOR reader out deliberately, and the consequence
  // — enrolment trusts the authenticated channel rather than proving
  // possession — is recorded on `Accounts::enroll_passkey`.
  var enrol = function () {
    var box = document.getElementById('enrol');
    var go = document.getElementById('enrol-go');
    if (!box || !go) return;
    var say = sayer('enrol-said');
    box.hidden = false;
    var user = box.dataset.user;
    go.addEventListener('click', function () {
      say('Follow your browser’s prompt...');
      settle(
        navigator.credentials.create({
          publicKey: {
            rp: { name: 'choir' },
            user: { id: new TextEncoder().encode(user), name: user, displayName: user },
            challenge: crypto.getRandomValues(new Uint8Array(32)),
            // ES256 only, because it is the one scheme the node
            // verifies. Offering a second algorithm here would enrol
            // keys refused at first use, which is the failure
            // enrolment-time validation exists to prevent — and the
            // test that holds this line greps for the identifiers, so
            // it stays out of the prose.
            pubKeyCredParams: [{ type: 'public-key', alg: -7 }],
            // Discoverable, because sign-in offers no username to look
            // a credential up by: the assertion's credential id is the
            // only name in it. Both spellings, since the older one is
            // what browsers predating `residentKey` understand and the
            // two disagreeing is how a credential gets enrolled that
            // can approve an operation but cannot open a session.
            authenticatorSelection: {
              userVerification: 'preferred',
              residentKey: 'required',
              requireResidentKey: true
            },
            timeout: 120000
          }
        }).then(function (c) {
          var spki = c.response.getPublicKey && c.response.getPublicKey();
          if (!spki) throw new Error('this browser did not return a public key');
          var label = document.getElementById('enrol-label').value || 'passkey';
          return post('/api/accounts/passkey', {
            credential_id: c.id, public_key: b64url(spki), label: label
          });
        }),
        say,
        'The node refused it: ',
        'Not enrolled: '
      );
    });
  };

  // Signing in. The fourth ceremony, and the only one that runs for
  // somebody the node has not identified yet: it is the page served in
  // place of the browser's own credential dialog, which is chrome no
  // page can style, explain, or offer a passkey through.
  //
  // No username is collected. The assertion carries a credential id and
  // the node looks the account up from it, which is why enrolment above
  // asks for a discoverable credential.
  var signin = function () {
    var box = document.getElementById('signin');
    var go = document.getElementById('signin-go');
    if (!box || !go) return;
    var say = sayer('signin-said');
    box.hidden = false;
    go.addEventListener('click', function () {
      say('Follow your browser\u2019s prompt...');
      // The challenge is minted per attempt and spent on use, so a page
      // left open does not accumulate signable bytes.
      fetch('/api/signin/challenge', { method: 'POST', credentials: 'same-origin' })
        .then(function (r) {
          if (!r.ok) throw new Error('the node issued no challenge');
          return r.json();
        })
        .then(function (issued) {
          return assert(issued.challenge);
        })
        .then(function (c) {
          return post('/api/signin', {
            key_id: c.id,
            scheme: 2,
            signature_hex: hex(c.response.signature),
            authenticator_data_hex: hex(c.response.authenticatorData),
            client_data_json_hex: hex(c.response.clientDataJSON)
          });
        })
        .then(function (r) {
          if (r.ok) {
            // Back to whatever was being asked for, which the node put
            // on the element rather than this file reading the query
            // string: one place decides where a sign-in returns to.
            location.assign(box.dataset.next || '/');
            return;
          }
          return r.text().then(function (t) { say('Not signed in: ' + t); });
        })
        .catch(function (e) { say('Not signed in: ' + e.message); });
    });
  };

  // How many leading zero bits a digest shows, tested against what the
  // node asked for. Byte at a time, because the answer is decided in the
  // first non-zero byte and nothing here should be clever.
  var leading = function (bytes, bits) {
    var whole = bits >> 3;
    for (var i = 0; i < whole; i++) if (bytes[i] !== 0) return false;
    var rest = bits & 7;
    return rest === 0 || (bytes[whole] >> (8 - rest)) === 0;
  };

  // D72's cost, paid here. `crypto.subtle.digest` is asynchronous per
  // call, so a naive loop would be both slow and a frozen tab; this hashes
  // a batch at a time, reports how far it has got, and yields to the event
  // loop between batches so the page keeps repainting.
  //
  // The preimage is `challenge:nonce`, which is the spelling
  // `choir_node::work::preimage` writes on the other side. A disagreement
  // about that colon looks exactly like a browser that cannot compute.
  var stamp = function (challenge, bits, progress) {
    var enc = new TextEncoder();
    var nonce = 0;
    var step = function () {
      var batch = [];
      for (var i = 0; i < 256; i++) batch.push(nonce + i);
      return Promise.all(batch.map(function (n) {
        return crypto.subtle.digest('SHA-256', enc.encode(challenge + ':' + n));
      })).then(function (digests) {
        for (var j = 0; j < digests.length; j++) {
          if (leading(new Uint8Array(digests[j]), bits)) return String(batch[j]);
        }
        nonce += batch.length;
        progress(nonce);
        return new Promise(function (done) { setTimeout(done, 0); }).then(step);
      });
    };
    return step();
  };

  // Asking for access (D72). The form is rendered hidden and unhidden
  // here, so a reader without script is never shown a button that cannot
  // work -- they get the `<noscript>` sentence and the operator's address
  // instead.
  //
  // On success this navigates to the claim link rather than printing it.
  // The address bar is the one place a person already knows how to
  // bookmark, and the page it lands on explains itself; a link rendered
  // into a paragraph by this file would be the only copy of the request
  // living somewhere nobody thinks to save.
  var ask = function () {
    var form = document.getElementById('ask');
    var go = document.getElementById('ask-go');
    if (!form || !go) return;
    var say = sayer('ask-said');
    if (!window.crypto || !crypto.subtle) {
      // Every other refusal in this file is a sentence in the status
      // line; this one has to leave the form hidden, because the button
      // genuinely cannot be made to work here.
      say('This browser cannot do the arithmetic this node asks for. Use the address above.');
      return;
    }
    form.hidden = false;
    go.addEventListener('click', function () {
      var name = document.getElementById('ask-name').value.trim();
      if (!name) { say('Say what to call you.'); return; }
      go.disabled = true;
      say('Working...');
      fetch('/api/access/challenge', { method: 'POST', credentials: 'same-origin' })
        .then(function (r) {
          if (!r.ok) throw new Error('the node issued no challenge');
          return r.json();
        })
        .then(function (issued) {
          return stamp(issued.challenge, issued.bits, function (tried) {
            say('Working... ' + tried.toLocaleString() + ' tries');
          }).then(function (nonce) {
            return post('/api/access', {
              display_name: name,
              about: document.getElementById('ask-about').value,
              challenge: issued.challenge,
              nonce: nonce
            });
          });
        })
        .then(function (r) {
          return r.json().then(function (answer) {
            if (r.ok && answer.join_url) { location.assign(answer.join_url); return; }
            go.disabled = false;
            say(answer.error || 'That was not accepted.');
          });
        })
        .catch(function (e) { go.disabled = false; say('Not asked: ' + e.message); });
    });
  };

  // D72's ask needs no authenticator, so it runs first and
  // unconditionally.
  ask();
  if (!webauthn) return;
  verdict();
  comment();
  enrol();
  signin();
})();
