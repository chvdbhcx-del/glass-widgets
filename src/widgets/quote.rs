//! Quote of the Day: M = one quote (large light text, accent opening mark, author in dim caps); L = the quote, a hairline and
//! a preview of tomorrow's quote. Offline, deterministic: the quote is picked from a built-in list by the day number.
//! Click = the next quote (session only, the card returns to today's quote after a restart).
//! The list holds short sayings of authors who are long out of copyright; wording is a plain Russian rendition.
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;

/// (text, author)
const QUOTES: &[(&str, &str)] = &[
    // Марк Аврелий
    ("Препятствие на пути становится самим путём.", "Марк Аврелий"),
    ("Лучший способ отомстить — не уподобляться обидчику.", "Марк Аврелий"),
    ("Каждый человек стоит ровно столько, сколько стоят заботы, которыми он занят.", "Марк Аврелий"),
    ("Живи каждый день так, словно он последний.", "Марк Аврелий"),
    ("Что не приносит пользы рою, не приносит пользы и пчеле.", "Марк Аврелий"),
    ("Наша жизнь такова, какой её делают наши мысли.", "Марк Аврелий"),
    ("Не рассуждай о том, каким должен быть хороший человек. Будь им.", "Марк Аврелий"),
    ("Если что-то трудно для тебя, не думай, что оно невозможно для человека.", "Марк Аврелий"),
    // Сенека
    ("Пока мы откладываем жизнь, она проходит.", "Сенека"),
    ("Не потому мы не решаемся, что трудно; трудно оттого, что мы не решаемся.", "Сенека"),
    ("Кто не знает, в какую гавань плывёт, для того нет попутного ветра.", "Сенека"),
    ("Не тот беден, кто мало имеет, а тот, кто жаждет большего.", "Сенека"),
    ("Жизнь достаточно длинна, если уметь ею пользоваться.", "Сенека"),
    ("Где есть человек, там есть место для добра.", "Сенека"),
    ("Важно не то, как долго ты живёшь, а то, как ты живёшь.", "Сенека"),
    ("Мы чаще страдаем в воображении, чем в действительности.", "Сенека"),
    ("Счастлив тот, кто доволен настоящим.", "Сенека"),
    ("Огонь испытывает золото, а несчастье — сильных людей.", "Сенека"),
    // Эпиктет
    ("Нас тревожат не вещи, а наши мнения о вещах.", "Эпиктет"),
    ("Не требуй, чтобы всё шло, как хочешь ты; желай, чтобы всё шло, как идёт, — и будешь спокоен.", "Эпиктет"),
    ("Одно зависит от нас, другое — не зависит.", "Эпиктет"),
    ("Хочешь быть писателем — пиши.", "Эпиктет"),
    ("Не говори «я потерял», говори «я вернул».", "Эпиктет"),
    ("Обстоятельства показывают, каков человек.", "Эпиктет"),
    // Пушкин
    ("Привычка свыше нам дана: замена счастию она.", "А. С. Пушкин"),
    ("Мы почитаем всех нулями, а единицами — себя.", "А. С. Пушкин"),
    ("Что пройдёт, то будет мило.", "А. С. Пушкин"),
    ("Ах, обмануть меня нетрудно! Я сам обманываться рад!", "А. С. Пушкин"),
    ("Блажен, кто смолоду был молод, блажен, кто вовремя созрел.", "А. С. Пушкин"),
    ("Гений и злодейство — две вещи несовместные.", "А. С. Пушкин"),
    ("Поэзия, прости Господи, должна быть глуповата.", "А. С. Пушкин"),
    ("Любви все возрасты покорны.", "А. С. Пушкин"),
    ("Служенье муз не терпит суеты.", "А. С. Пушкин"),
    ("Если жизнь тебя обманет, не печалься, не сердись!", "А. С. Пушкин"),
    // Лермонтов
    ("Страсти — не что иное, как идеи при первом своём развитии.", "М. Ю. Лермонтов"),
    ("Мы почти всегда извиняем то, что понимаем.", "М. Ю. Лермонтов"),
    // Толстой
    ("Все счастливые семьи похожи друг на друга, каждая несчастливая семья несчастлива по-своему.", "Л. Н. Толстой"),
    ("Все думают об изменении человечества, и никто не думает о том, как изменить себя.", "Л. Н. Толстой"),
    ("Нет величия там, где нет простоты, добра и правды.", "Л. Н. Толстой"),
    ("Самое важное время — сейчас, самый нужный человек — тот, кто рядом, самое важное дело — сделать ему добро.", "Л. Н. Толстой"),
    ("Чтобы жить честно, надо рваться, путаться, биться, ошибаться.", "Л. Н. Толстой"),
    ("Терпение и время — вот мои воины.", "Л. Н. Толстой"),
    // Лао-цзы
    ("Путь в тысячу ли начинается с первого шага.", "Лао-цзы"),
    ("Знающий других мудр; знающий себя просвещён.", "Лао-цзы"),
    ("Побеждающий других силён; победивший себя могуществен.", "Лао-цзы"),
    ("Мягкое побеждает твёрдое, слабое побеждает сильное.", "Лао-цзы"),
    ("Кто доволен малым, тот богат.", "Лао-цзы"),
    ("Великое начинается с малого.", "Лао-цзы"),
    ("Кто знает, тот не говорит; кто говорит, тот не знает.", "Лао-цзы"),
    ("Высшее благо подобно воде: она приносит пользу всему и не борется.", "Лао-цзы"),
    // Конфуций
    ("Не делай другим того, чего не желаешь себе.", "Конфуций"),
    ("Учиться и не размышлять — напрасно терять время; размышлять и не учиться — губительно.", "Конфуций"),
    ("Не беда, что люди тебя не знают; беда, если ты не знаешь людей.", "Конфуций"),
    ("Благородный муж стремится к согласию, а не к единомыслию.", "Конфуций"),
    ("Увидев достойного, стремись сравняться с ним; увидев недостойного, проверь себя.", "Конфуций"),
    ("Знать, что знаешь, и знать, чего не знаешь, — вот истинное знание.", "Конфуций"),
    // Франклин
    ("Время — деньги.", "Бенджамин Франклин"),
    ("Рано ложиться и рано вставать — значит быть здоровым, богатым и мудрым.", "Бенджамин Франклин"),
    ("Вложения в знания приносят наилучший доход.", "Бенджамин Франклин"),
    ("Маленькая течь может потопить большой корабль.", "Бенджамин Франклин"),
    ("Не откладывай на завтра то, что можно сделать сегодня.", "Бенджамин Франклин"),
    ("Кто умеет терпеть, тот может получить всё, чего пожелает.", "Бенджамин Франклин"),
    ("Хорошо сделанное лучше, чем хорошо сказанное.", "Бенджамин Франклин"),
    // Античность
    ("Я знаю, что ничего не знаю.", "Сократ"),
    ("Неисследованная жизнь не стоит того, чтобы её прожить.", "Сократ"),
    ("Единственное благо — знание, единственное зло — невежество.", "Сократ"),
    ("Платон мне друг, но истина дороже.", "Аристотель"),
    ("Человек по природе — существо общественное.", "Аристотель"),
    ("Надежда — это сон наяву.", "Аристотель"),
    ("Корень учения горек, но плоды его сладки.", "Аристотель"),
    ("Всё течёт.", "Гераклит"),
    ("В одну реку нельзя войти дважды.", "Гераклит"),
    ("Характер человека — его судьба.", "Гераклит"),
    ("Начало — самая важная часть любого дела.", "Платон"),
    ("Познание начинается с удивления.", "Платон"),
    ("Пока дышу — надеюсь.", "Цицерон"),
    ("Жить — значит мыслить.", "Цицерон"),
    ("Благодарность — не только величайшая из добродетелей, но и родительница всех остальных.", "Цицерон"),
    ("Лови день и как можно меньше верь грядущему.", "Гораций"),
    ("Дерзай знать.", "Гораций"),
    // Философы нового времени
    ("Имей мужество пользоваться собственным умом.", "Иммануил Кант"),
    ("Две вещи наполняют душу удивлением и благоговением: звёздное небо надо мной и нравственный закон во мне.", "Иммануил Кант"),
    ("Я мыслю, следовательно, существую.", "Рене Декарт"),
    ("Не смеяться, не плакать, не ненавидеть, а понимать.", "Спиноза"),
    ("Что нас не убивает, делает нас сильнее.", "Фридрих Ницше"),
    ("Кто знает, зачем жить, тот выдержит почти любое как.", "Фридрих Ницше"),
    ("Человек — всего лишь тростник, самый слабый в природе, но это тростник мыслящий.", "Блез Паскаль"),
    ("У сердца свои доводы, которых разум не знает.", "Блез Паскаль"),
    ("Что я знаю?", "Мишель Монтень"),
    ("Величайшая вещь на свете — умение принадлежать самому себе.", "Мишель Монтень"),
    ("У нас достаточно сил, чтобы переносить несчастья ближнего.", "Ларошфуко"),
    ("Только великие люди имеют великие недостатки.", "Ларошфуко"),
    ("Знание — сила.", "Фрэнсис Бэкон"),
    ("Природу побеждают, только подчиняясь ей.", "Фрэнсис Бэкон"),
    ("Лучшее — враг хорошего.", "Вольтер"),
    ("Работа избавляет нас от трёх великих зол: скуки, порока и нужды.", "Вольтер"),
    ("Чтобы иметь друга, нужно самому быть им.", "Ральф Эмерсон"),
    ("Ничто великое не совершалось без энтузиазма.", "Ральф Эмерсон"),
    ("Простота, простота и ещё раз простота!", "Генри Торо"),
    ("Важно не то, на что ты смотришь, а то, что ты видишь.", "Генри Торо"),
    ("Мужество — это сопротивление страху и овладение им, а не отсутствие страха.", "Марк Твен"),
    ("Слухи о моей смерти сильно преувеличены.", "Марк Твен"),
    ("Я могу устоять против всего, кроме искушения.", "Оскар Уайльд"),
    ("Опыт — это название, которое каждый даёт своим ошибкам.", "Оскар Уайльд"),
    ("Знай врага и знай себя — и в сотне битв не будешь в опасности.", "Сунь-цзы"),
    ("Высшее искусство войны — победить врага без сражения.", "Сунь-цзы"),
    // Гёте, Шекспир
    ("Знать мало — надо применять; желать мало — надо делать.", "Иоганн Гёте"),
    ("Теория, мой друг, суха, но зеленеет вечное дерево жизни.", "Иоганн Гёте"),
    ("Лишь тот достоин счастья и свободы, кто каждый день идёт за них на бой.", "Иоганн Гёте"),
    ("Мгновенье, остановись, ты прекрасно!", "Иоганн Гёте"),
    ("Весь мир — театр, в нём женщины, мужчины — все актёры.", "Уильям Шекспир"),
    ("Быть или не быть — вот в чём вопрос.", "Уильям Шекспир"),
    ("Нет ничего ни хорошего, ни плохого — всё зависит от нашего мышления.", "Уильям Шекспир"),
    ("Краткость — душа остроумия.", "Уильям Шекспир"),
    // Русская классика
    ("В человеке должно быть прекрасно всё: и лицо, и одежда, и душа, и мысли.", "А. П. Чехов"),
    ("Краткость — сестра таланта.", "А. П. Чехов"),
    ("Человеку нужно не три аршина земли, а весь земной шар.", "А. П. Чехов"),
    ("Красота спасёт мир.", "Ф. М. Достоевский"),
    ("Тайна бытия человеческого не в том, чтобы только жить, а в том, для чего жить.", "Ф. М. Достоевский"),
    ("Человек есть тайна. Её надо разгадать.", "Ф. М. Достоевский"),
    ("Нет уз святее товарищества.", "Н. В. Гоголь"),
    ("А Васька слушает да ест.", "И. А. Крылов"),
    ("Беда, коль пироги начнёт печи сапожник, а сапоги тачать пирожник.", "И. А. Крылов"),
    ("У сильного всегда бессильный виноват.", "И. А. Крылов"),
    ("Счастливые часов не наблюдают.", "А. С. Грибоедов"),
    ("Служить бы рад, прислуживаться тошно.", "А. С. Грибоедов"),
    ("Науки юношей питают, отраду старым подают.", "М. В. Ломоносов"),
    ("Тяжело в ученье — легко в походе.", "А. В. Суворов"),
    ("Зри в корень!", "Козьма Прутков"),
    ("Никто не обнимет необъятного.", "Козьма Прутков"),
    ("Если хочешь быть счастливым, будь им.", "Козьма Прутков"),
    ("Усердие всё превозмогает.", "Козьма Прутков"),
    ("Природа не храм, а мастерская, и человек в ней работник.", "И. С. Тургенев"),
    ("Поэтом можешь ты не быть, но гражданином быть обязан.", "Н. А. Некрасов"),
    ("Нам не дано предугадать, как слово наше отзовётся.", "Ф. И. Тютчев"),
    ("Молчи, скрывайся и таи и чувства и мечты свои.", "Ф. И. Тютчев"),
    ("Умом Россию не понять, аршином общим не измерить.", "Ф. И. Тютчев"),
    ("Человек — это звучит гордо.", "М. Горький"),
    ("Рождённый ползать летать не может.", "М. Горький"),
];

/// The English edition of `QUOTES`: the SAME quotes in the SAME order (the day picks one index for both languages), in the usual English
/// renditions where one exists, otherwise a plain translation.
const QUOTES_EN: &[(&str, &str)] = &[
    // Marcus Aurelius
    ("An obstacle in the path becomes the path itself.", "Marcus Aurelius"),
    ("The best revenge is not to be like your enemy.", "Marcus Aurelius"),
    ("A person is worth exactly as much as the things he cares about.", "Marcus Aurelius"),
    ("Live every day as if it were your last.", "Marcus Aurelius"),
    ("What is not good for the swarm is not good for the bee.", "Marcus Aurelius"),
    ("Our life is what our thoughts make it.", "Marcus Aurelius"),
    ("Waste no more time arguing about what a good man should be. Be one.", "Marcus Aurelius"),
    ("If something is hard for you, do not think it impossible for a human being.", "Marcus Aurelius"),
    // Seneca
    ("While we are postponing, life speeds by.", "Seneca"),
    ("It is not because things are difficult that we do not dare; it is because we do not dare that they are difficult.", "Seneca"),
    ("If a man does not know to which port he is sailing, no wind is favourable.", "Seneca"),
    ("It is not the man who has too little, but the man who craves more, that is poor.", "Seneca"),
    ("Life is long enough, if you know how to use it.", "Seneca"),
    ("Wherever there is a human being, there is an opportunity for kindness.", "Seneca"),
    ("It is not how long you live, but how you live that matters.", "Seneca"),
    ("We suffer more often in imagination than in reality.", "Seneca"),
    ("Happy is the one who is content with the present.", "Seneca"),
    ("Fire tests gold; misfortune tests strong men.", "Seneca"),
    // Epictetus
    ("People are disturbed not by things, but by the views they take of them.", "Epictetus"),
    ("Don't demand that things happen as you wish; wish that they happen as they do, and you will be at peace.", "Epictetus"),
    ("Some things are up to us, and some are not.", "Epictetus"),
    ("If you want to be a writer, write.", "Epictetus"),
    ("Never say \u{201c}I have lost it\u{201d}, but \u{201c}I have given it back.\u{201d}", "Epictetus"),
    ("Circumstances show what a person is made of.", "Epictetus"),
    // Pushkin
    ("Habit is given to us from above: it is a substitute for happiness.", "A. S. Pushkin"),
    ("We regard everyone as a zero, and ourselves as a one.", "A. S. Pushkin"),
    ("What passes will be dear to us.", "A. S. Pushkin"),
    ("Oh, it is easy to deceive me! I am glad to be deceived myself!", "A. S. Pushkin"),
    ("Blessed is he who was young in his youth, blessed is he who ripened in time.", "A. S. Pushkin"),
    ("Genius and villainy are two things incompatible.", "A. S. Pushkin"),
    ("Poetry, God forgive me, has to be a little silly.", "A. S. Pushkin"),
    ("All ages bow to love.", "A. S. Pushkin"),
    ("The service of the Muses does not tolerate bustle.", "A. S. Pushkin"),
    ("If life deceives you, do not grieve, do not be angry!", "A. S. Pushkin"),
    // Lermontov
    ("Passions are nothing but ideas in their first stage of development.", "M. Yu. Lermontov"),
    ("We almost always forgive what we understand.", "M. Yu. Lermontov"),
    // Tolstoy
    ("All happy families are alike; each unhappy family is unhappy in its own way.", "L. N. Tolstoy"),
    ("Everyone thinks of changing the world, but no one thinks of changing himself.", "L. N. Tolstoy"),
    ("There is no greatness where there is no simplicity, goodness and truth.", "L. N. Tolstoy"),
    ("The most important time is now, the most important person is the one beside you, the most important deed is to do them good.", "L. N. Tolstoy"),
    ("To live honestly, one must struggle, get confused, fight, make mistakes.", "L. N. Tolstoy"),
    ("Patience and time are my warriors.", "L. N. Tolstoy"),
    // Lao Tzu
    ("A journey of a thousand miles begins with a single step.", "Lao Tzu"),
    ("He who knows others is wise; he who knows himself is enlightened.", "Lao Tzu"),
    ("He who conquers others is strong; he who conquers himself is mighty.", "Lao Tzu"),
    ("The soft overcomes the hard, the weak overcomes the strong.", "Lao Tzu"),
    ("He who is content with little is rich.", "Lao Tzu"),
    ("The great begins with the small.", "Lao Tzu"),
    ("Those who know do not speak; those who speak do not know.", "Lao Tzu"),
    ("The highest good is like water: it benefits all things and does not compete.", "Lao Tzu"),
    // Confucius
    ("Do not do to others what you do not want done to yourself.", "Confucius"),
    ("To learn without thinking is a waste; to think without learning is dangerous.", "Confucius"),
    ("Do not worry that people do not know you; worry that you do not know people.", "Confucius"),
    ("The noble man seeks harmony, not sameness.", "Confucius"),
    ("Seeing a worthy person, strive to equal them; seeing an unworthy one, examine yourself.", "Confucius"),
    ("To know what you know and to know what you do not know: that is true knowledge.", "Confucius"),
    // Franklin
    ("Time is money.", "Benjamin Franklin"),
    ("Early to bed and early to rise makes a man healthy, wealthy and wise.", "Benjamin Franklin"),
    ("An investment in knowledge pays the best interest.", "Benjamin Franklin"),
    ("A small leak will sink a great ship.", "Benjamin Franklin"),
    ("Never leave till tomorrow what you can do today.", "Benjamin Franklin"),
    ("He that can have patience can have what he will.", "Benjamin Franklin"),
    ("Well done is better than well said.", "Benjamin Franklin"),
    // Antiquity
    ("I know that I know nothing.", "Socrates"),
    ("The unexamined life is not worth living.", "Socrates"),
    ("There is only one good, knowledge, and one evil, ignorance.", "Socrates"),
    ("Plato is dear to me, but truth is dearer.", "Aristotle"),
    ("Man is by nature a social animal.", "Aristotle"),
    ("Hope is a waking dream.", "Aristotle"),
    ("The roots of education are bitter, but the fruit is sweet.", "Aristotle"),
    ("Everything flows.", "Heraclitus"),
    ("You cannot step into the same river twice.", "Heraclitus"),
    ("A person's character is their destiny.", "Heraclitus"),
    ("The beginning is the most important part of any work.", "Plato"),
    ("Wisdom begins in wonder.", "Plato"),
    ("While I breathe, I hope.", "Cicero"),
    ("To live is to think.", "Cicero"),
    ("Gratitude is not only the greatest of virtues, but the parent of all the others.", "Cicero"),
    ("Seize the day, and trust as little as possible in tomorrow.", "Horace"),
    ("Dare to know.", "Horace"),
    // Modern philosophers
    ("Have the courage to use your own understanding.", "Immanuel Kant"),
    ("Two things fill the mind with wonder and awe: the starry heavens above me and the moral law within me.", "Immanuel Kant"),
    ("I think, therefore I am.", "René Descartes"),
    ("Not to laugh, not to lament, not to detest, but to understand.", "Spinoza"),
    ("What does not kill me makes me stronger.", "Friedrich Nietzsche"),
    ("He who has a why to live can bear almost any how.", "Friedrich Nietzsche"),
    ("Man is only a reed, the weakest in nature, but he is a thinking reed.", "Blaise Pascal"),
    ("The heart has its reasons, which reason does not know.", "Blaise Pascal"),
    ("What do I know?", "Michel de Montaigne"),
    ("The greatest thing in the world is to know how to belong to oneself.", "Michel de Montaigne"),
    ("We all have strength enough to endure the misfortunes of others.", "La Rochefoucauld"),
    ("Only great men have great faults.", "La Rochefoucauld"),
    ("Knowledge is power.", "Francis Bacon"),
    ("Nature is conquered only by obeying her.", "Francis Bacon"),
    ("The best is the enemy of the good.", "Voltaire"),
    ("Work keeps at bay three great evils: boredom, vice and need.", "Voltaire"),
    ("To have a friend, you must be one.", "Ralph Emerson"),
    ("Nothing great was ever achieved without enthusiasm.", "Ralph Emerson"),
    ("Simplicity, simplicity, simplicity!", "Henry Thoreau"),
    ("It is not what you look at that matters, it is what you see.", "Henry Thoreau"),
    ("Courage is resistance to fear, mastery of fear, not absence of fear.", "Mark Twain"),
    ("The reports of my death are greatly exaggerated.", "Mark Twain"),
    ("I can resist everything except temptation.", "Oscar Wilde"),
    ("Experience is the name everyone gives to their mistakes.", "Oscar Wilde"),
    ("Know your enemy and know yourself, and in a hundred battles you will never be in peril.", "Sun Tzu"),
    ("The supreme art of war is to subdue the enemy without fighting.", "Sun Tzu"),
    // Goethe, Shakespeare
    ("Knowing is not enough, we must apply; willing is not enough, we must do.", "Johann Goethe"),
    ("All theory, dear friend, is grey, but the golden tree of life is green.", "Johann Goethe"),
    ("He alone deserves freedom and life who must daily conquer them anew.", "Johann Goethe"),
    ("Stay a while, you are so beautiful!", "Johann Goethe"),
    ("All the world's a stage, and all the men and women merely players.", "William Shakespeare"),
    ("To be, or not to be, that is the question.", "William Shakespeare"),
    ("There is nothing either good or bad, but thinking makes it so.", "William Shakespeare"),
    ("Brevity is the soul of wit.", "William Shakespeare"),
    // Russian classics
    ("Everything about a person should be beautiful: the face, the clothes, the soul, the thoughts.", "A. P. Chekhov"),
    ("Brevity is the sister of talent.", "A. P. Chekhov"),
    ("A man needs not six feet of earth, but the whole globe.", "A. P. Chekhov"),
    ("Beauty will save the world.", "F. M. Dostoevsky"),
    ("The mystery of human existence lies not in just staying alive, but in finding something to live for.", "F. M. Dostoevsky"),
    ("Man is a mystery. It must be solved.", "F. M. Dostoevsky"),
    ("There are no bonds holier than comradeship.", "N. V. Gogol"),
    ("And Vaska listens on, and eats.", "I. A. Krylov"),
    ("Trouble comes when the cobbler starts baking pies and the pie-maker starts mending boots.", "I. A. Krylov"),
    ("In the eyes of the strong, the powerless are always to blame.", "I. A. Krylov"),
    ("The happy do not watch the clock.", "A. S. Griboyedov"),
    ("I would gladly serve, but being servile makes me sick.", "A. S. Griboyedov"),
    ("Sciences nourish the young and give delight to the old.", "M. V. Lomonosov"),
    ("Hard in training, easy in battle.", "A. V. Suvorov"),
    ("Look to the root!", "Kozma Prutkov"),
    ("No one can embrace the unembraceable.", "Kozma Prutkov"),
    ("If you want to be happy, be.", "Kozma Prutkov"),
    ("Diligence overcomes everything.", "Kozma Prutkov"),
    ("Nature is not a temple but a workshop, and man is the workman in it.", "I. S. Turgenev"),
    ("You need not be a poet, but you must be a citizen.", "N. A. Nekrasov"),
    ("We cannot foresee how our word will echo back.", "F. I. Tyutchev"),
    ("Be silent, hide yourself, and conceal your feelings and your dreams.", "F. I. Tyutchev"),
    ("Russia cannot be understood with the mind, nor measured by a common yardstick.", "F. I. Tyutchev"),
    ("Man: that sounds proud.", "M. Gorky"),
    ("One born to crawl cannot fly.", "M. Gorky"),
];

/// Quote index for a day number: a fixed-stride walk through the list, so neighbouring days are unrelated and
/// every quote comes up once per `QUOTES.len()` days (the stride is coprime to the length, see the test).
fn pick(day: i64, len: usize) -> usize {
    ((day.rem_euclid(len as i64) as usize) * 37 + 11) % len
}

#[derive(Default)]
struct Quote {
    /// days added by clicking (session only)
    skip: i64,
    ver: u64,
}

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "quote",
        name: ("Цитата дня", "Quote of the Day"),
        desc: ("Короткая цитата на сегодня", "A short quote for today"),
        group: "info",
        icon: Glyph::Star,
        sizes: &[Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Quote::default()),
    });
}

impl Quote {
    fn day(&self, cx: &Cx, extra: i64) -> (&'static str, &'static str) {
        let d = tz::days_from_civil(cx.now.y, cx.now.mo, cx.now.d) + self.skip + extra;
        // one index for both editions: the same quote comes up on the same day in either language
        (if cx.lang == Lang::Ru { QUOTES } else { QUOTES_EN })[pick(d, QUOTES.len())]
    }
}

/// Largest size of `sizes` (descending) at which `text` wrapped to `w` fits into `h`.
fn fit(cx: &Cx, text: &str, w: f32, h: f32, sizes: &[f32]) -> f32 {
    for &s in sizes {
        if cx.measure(text, Style::new(s, 300).wrap(), w).1 <= h {
            return s;
        }
    }
    sizes.last().copied().unwrap_or(14.0)
}

/// Cut `text` at a word boundary and add an ellipsis until it fits `h` when wrapped to `w`.
fn clip_words(cx: &Cx, text: &str, st: Style, w: f32, h: f32) -> String {
    if cx.measure(text, st, w).1 <= h {
        return text.to_string();
    }
    let mut words: Vec<&str> = text.split(' ').collect();
    while words.len() > 1 {
        words.pop();
        let s = format!("{}…", words.join(" ").trim_end_matches([',', ';', ':', ' ']));
        if cx.measure(&s, st, w).1 <= h {
            return s;
        }
    }
    text.to_string()
}

fn author_line(cx: &Cx, r: Rect, who: &str, color: Color) {
    cx.text(r, &format!("— {}", who.to_uppercase()), Style::new(11.0, 700).track(0.7).ellipsis(), color);
}

impl Widget for Quote {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        let (text, who) = self.day(cx, 0);
        const SIZES: [f32; 8] = [30.0, 27.0, 24.0, 22.0, 20.0, 18.0, 16.0, 14.0];
        match cx.size {
            Size::L => {
                cx.text(Rect::new(16.0, 16.0, 200.0, 16.0), &cx.t("ЦИТАТА ДНЯ", "QUOTE OF THE DAY"), Style::LABEL, accent());
                let date = format!("{} {}", cx.now.d, month_genitive(cx.lang, cx.now.mo));
                cx.text(Rect::new(200.0, 16.0, 140.0, 16.0), &date, Style::new(12.0, 500).right(), DIM);
                cx.text(Rect::new(12.0, 28.0, 80.0, 64.0), "❝", Style::new(64.0, 400), accent());
                let area = Rect::new(16.0, 92.0, 324.0, 136.0);
                let size = fit(cx, text, area.w, area.h, &SIZES);
                cx.text(area, text, Style::new(size, 300).wrap(), TEXT);
                author_line(cx, Rect::new(16.0, 232.0, 324.0, 16.0), who, DIM);
                cx.rect(Rect::new(16.0, 262.0, 324.0, 0.6), FILL);
                let (nt, nw) = self.day(cx, 1);
                cx.text(Rect::new(16.0, 270.0, 324.0, 15.0), &cx.t("ЗАВТРА", "TOMORROW"), Style::LABEL, FAINT);
                let st = Style::new(13.0, 400).wrap();
                let clipped = clip_words(cx, nt, st, 324.0, 36.0);
                cx.text(Rect::new(16.0, 290.0, 324.0, 38.0), &clipped, st, DIM);
                author_line(cx, Rect::new(16.0, 328.0, 324.0, 14.0), nw, FAINT);
            }
            _ => {
                cx.text(Rect::new(12.0, -2.0, 60.0, 56.0), "❝", Style::new(46.0, 400), accent());
                let area = Rect::new(16.0, 42.0, 324.0, 80.0);
                let size = fit(cx, text, area.w, area.h, &SIZES[2..]);
                cx.text(area, text, Style::new(size, 300).wrap(), TEXT);
                author_line(cx, Rect::new(16.0, 138.0, 324.0, 16.0), who, DIM);
            }
        }
        Ok(())
    }

    fn tick(&mut self, _cx: &Cx) -> Tick {
        Tick::Never
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if ev.clicked(cx.bounds()) {
            self.skip += 1;
            self.ver += 1;
            return true;
        }
        false
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_quote_once_per_cycle() {
        let n = QUOTES.len();
        assert!(n >= 100, "{n}");
        let mut seen = vec![false; n];
        for d in 0..n as i64 {
            let i = pick(20_000 + d, n);
            assert!(!seen[i], "repeat at {d}");
            seen[i] = true;
        }
        assert!(pick(5, n) != pick(6, n));
        assert_eq!(pick(-3, n), pick(-3 + n as i64, n));
    }

    #[test]
    fn english_edition_matches_the_russian_one() {
        assert_eq!(QUOTES_EN.len(), QUOTES.len());
        for (i, ((t, a), (rt, ra))) in QUOTES_EN.iter().zip(QUOTES).enumerate() {
            assert!(!t.is_empty() && !a.is_empty(), "{i}");
            assert!(![t, a].iter().any(|s| s.chars().any(|c| ('\u{0400}'..='\u{04ff}').contains(&c))), "Cyrillic in the English edition: {t}");
            assert_ne!(t, rt, "{i} not translated");
            assert!(t.chars().count() <= 130, "too long: {t}");
            // the same author, in the same slot (a renamed author would mean the lists drifted apart)
            assert_eq!(a.chars().filter(|c| *c == '.').count() > 0, ra.chars().filter(|c| *c == '.').count() > 0, "{i}: {a} vs {ra}");
        }
    }

    #[test]
    fn list_is_clean() {
        for (t, a) in QUOTES {
            assert!(!t.is_empty() && !a.is_empty());
            assert!(t.chars().count() <= 130, "too long: {t}");
        }
    }
}
