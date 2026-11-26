- **A scroll bar can stay up whenever its content overflows, and take its own colour while
  held.** `ScrollBarStyle::always_shown` draws the bar as a classic one, shown while the content
  is longer than the viewport whatever the "Show scroll bars" preference, for a desktop whose
  look always draws its bars. `ScrollBarStyle::thumb_held` colours the thumb while a press holds
  it or the track (an accent while it is dragged); without it a held thumb keeps the hover colour.
