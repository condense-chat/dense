if (navigator.clipboard && window.isSecureContext) {
  for (const block of document.querySelectorAll('.cmd')) {
    const button = document.createElement('button');
    button.className = 'copy';
    button.type = 'button';
    button.textContent = 'Copy';
    button.setAttribute('aria-label', 'Copy command');
    button.addEventListener('click', async () => {
      try {
        await navigator.clipboard.writeText(block.querySelector('code').textContent);
        button.textContent = 'Copied';
      } catch {
        button.textContent = 'Select text';
      }
      setTimeout(() => { button.textContent = 'Copy'; }, 1600);
    });
    block.append(button);
  }
}
