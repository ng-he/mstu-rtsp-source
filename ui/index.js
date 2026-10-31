export default function mount(plugin) {
  const el = plugin.el

  /// The dot lights while frames are still arriving, not merely while the
  /// connection is up: a camera can be connected and sending nothing.
  let carried = 0

  plugin.fit(300)

  el('url').addEventListener('change', (event) => plugin.set('url', event.target.value.trim()))
  el('user').addEventListener('change', (event) => plugin.set('username', event.target.value))
  el('password').addEventListener('change', (event) => plugin.set('password', event.target.value))
  el('reconnect').addEventListener('change', (event) => plugin.set('reconnect', event.target.checked))
  el('redial').addEventListener('click', () => plugin.invoke('stream.reconnect'))

  plugin.onSettings((settings) => {
    el('url').value = settings.url ?? ''
    el('user').value = settings.username ?? ''
    el('password').value = settings.password ?? ''
    el('reconnect').checked = settings.reconnect ?? true
  })

  /// The codec and the address arrive as an event, since they are only known
  /// once the camera has been asked.
  plugin.onEvent('stream.changed', ({ stream_name, codec }) => {
    el('codec').textContent = codec ?? ''
    el('codec').hidden = !codec
    el('stream').textContent = stream_name ?? ''
  })

  plugin.onEvent('stream.ended', () => {
    el('codec').hidden = true
  })

  plugin.onLive(({ state, frames, dropped, bitrate, note }) => {
    el('state').textContent = state ?? 'idle'
    el('frames').textContent = frames ?? 0
    el('kbits').textContent = bitrate ?? 0
    el('loss').textContent = dropped ? `, ${dropped} dropped` : ''

    el('note').textContent = note ?? ''
    el('note').hidden = !note

    el('dot').classList.toggle('on', state === 'playing' && frames !== carried)
    el('redial').disabled = !el('url').value

    carried = frames ?? 0
  })

  plugin.onState(({ running }) => {
    if (!running) {
      el('dot').classList.remove('on')
      el('state').textContent = 'idle'
    }
  })
}
