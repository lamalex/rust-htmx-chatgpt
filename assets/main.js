document.body.addEventListener('htmx:afterOnLoad', function (event) {
    if (event.detail.xhr.status === 404) {
        var main = document.querySelector('main');
        main.innerHTML = event.detail.xhr.responseText;
    }
});