I hit the maximum number of tool iterations for this turn. Here is what
I did so far:

{% if tools_used %}
Tools called: {{ tools_used | join(", ") }}
{% endif %}

If you want me to continue, send another message.
